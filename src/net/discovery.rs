// Copyright 2015 click2stream, Inc.
// 
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
// 
//     http://www.apache.org/licenses/LICENSE-2.0
// 
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

///! Network scanner for RTSP streams.

use std::io;
use std::fmt;
use std::thread;
use std::result;

use std::fs::File;
use std::sync::Arc;
use std::error::Error;
use std::collections::HashSet;
use std::io::{BufReader, BufRead};
use std::fmt::{Display, Formatter};
use std::net::{IpAddr, SocketAddr, SocketAddrV4};

use net::rtsp;
use net::raw::pcap;

use net::rtsp::Client as RtspClient;
use net::raw::devices::EthernetDevice;
use net::raw::ether::MacAddr;
use net::raw::arp::scanner::Ipv4ArpScanner;
use net::raw::icmp::scanner::IcmpScanner;
use net::arrow::protocol::{Service, ScanReport};
use net::arrow::protocol::{HINFO_FLAG_ARP, HINFO_FLAG_ICMP};
use net::raw::tcp::scanner::{TcpPortScanner, PortCollection};
use net::rtsp::sdp::{SessionDescription, MediaType, RTPMap, FromAttribute};

static RTSP_PATH_FILE: &'static str = "/etc/arrow/rtsp-paths";

/// Discovery error.
#[derive(Debug, Clone)]
pub struct DiscoveryError {
    msg: String,
}

impl Error for DiscoveryError {
    fn description(&self) -> &str {
        &self.msg
    }
}

impl Display for DiscoveryError {
    fn fmt(&self, f: &mut Formatter) -> result::Result<(), fmt::Error> {
        f.write_str(&self.msg)
    }
}

impl From<String> for DiscoveryError {
    fn from(msg: String) -> DiscoveryError {
        DiscoveryError { msg: msg }
    }
}

impl<'a> From<&'a str> for DiscoveryError {
    fn from(msg: &'a str) -> DiscoveryError {
        DiscoveryError { msg: msg.to_string() }
    }
}

impl From<rtsp::RtspError> for DiscoveryError {
    fn from(err: rtsp::RtspError) -> DiscoveryError {
        DiscoveryError::from(format!("RTSP client error: {}", 
            err.description()))
    }
}

impl From<pcap::PcapError> for DiscoveryError {
    fn from(err: pcap::PcapError) -> DiscoveryError {
        DiscoveryError::from(format!("pcap error: {}", err.description()))
    }
}

impl From<io::Error> for DiscoveryError {
    fn from(err: io::Error) -> DiscoveryError {
        DiscoveryError::from(format!("IO error: {}", err.description()))
    }
}

/// Discovery result type alias.
pub type Result<T> = result::Result<T, DiscoveryError>;

/// Find all RTSP streams in all local networks.
pub fn find_rtsp_streams() -> Result<ScanReport> {
    let tc      = pcap::new_threading_context();
    let devices = EthernetDevice::list();
    
    let port_candidates = PortCollection::new()
        .add_all(&[554, 88, 81, 555, 7447, 8554, 7070, 10554, 80]);
    
    let mut threads = Vec::new();
    
    for dev in devices {
        let pc     = port_candidates.clone();
        let tc     = tc.clone();
        let handle = thread::spawn(move || {
            find_services(tc, &dev, &pc)
        });
        
        threads.push(handle);
    }
    
    let mut report = ScanReport::new();
    
    for handle in threads {
        if let Ok(res) = handle.join() {
            report.merge(try!(res));
        } else {
            return Err(DiscoveryError::from("port scanner thread panicked"));
        }
    }
    
    let rtsp_services = try!(find_rtsp_services(&report));
    
    let mut threads = Vec::new();
    let paths       = Arc::new(try!(load_rtsp_paths(RTSP_PATH_FILE)));
    
    for (mac, addr) in rtsp_services {
        let paths  = paths.clone();
        let handle = thread::spawn(move || {
            find_rtsp_paths(mac, addr, &paths)
        });
        threads.push(handle);
    }
    
    let mut services = Vec::new();
    
    for handle in threads {
        match handle.join() {
            Err(_) => return Err(DiscoveryError::from(
                "path testing thread panicked")),
            Ok(svcs) => services.extend(try!(svcs))
        }
    }
    
    for svc in services {
        report.add_service(svc);
    }
    
    Ok(report)
}

/// Load all known RTSP path variants from a given file.
fn load_rtsp_paths(file: &str) -> Result<Vec<String>> {
    let file      = try!(File::open(file));
    let breader   = BufReader::new(file);
    let mut paths = Vec::new();
    
    for line in breader.lines() {
        let path = try!(line);
        if !path.starts_with('#') {
            paths.push(path);
        }
    }
    
    Ok(paths)
}

/// Check if a given service is an RTSP service.
fn is_rtsp_service(addr: SocketAddr) -> Result<bool> {
    let mut client = try!(RtspClient::new(addr));
    client.set_timeout(Some(1000));
    Ok(client.options().is_ok())
}

/// Check if a given session description contains at least one H.264 or 
/// a general MPEG4 video stream.
fn is_supported_service(sdp: &[u8]) -> bool {
    if let Ok(sdp) = SessionDescription::parse(sdp) {
        let mut vcodecs   = HashSet::new();
        let video_streams = sdp.media_descriptions.into_iter()
            .filter(|md| md.media_type == MediaType::Video);
        
        for md in video_streams {
            for attr in md.attributes {
                if let Ok(rtpmap) = RTPMap::parse(&attr) {
                    vcodecs.insert(rtpmap.encoding.to_uppercase());
                }
            }
        }
        
        vcodecs.contains("H264") ||
            vcodecs.contains("H264-RCDO") ||
            vcodecs.contains("H264-SVC") ||
            vcodecs.contains("MP4V-ES") ||
            vcodecs.contains("MPEG4-GENERIC")
    } else {
        false
    }
}

/// RTSP DESCRIBE status.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum DescribeStatus {
    Ok,
    Locked,
    Unsupported,
    NotFound,
    Error
}

/// Get describe status code for a given RTSP service and path.
fn get_describe_status(addr: SocketAddr, path: &str) -> Result<DescribeStatus> {
    let mut client = try!(RtspClient::new(addr));
    client.set_timeout(Some(1000));
    if let Ok(response) = client.describe(path) {
        let header = response.header;
        let hipcam = match header.get_str("Server") {
            Some("HiIpcam/V100R003 VodServer/1.0.0") => true,
            Some("Hipcam RealServer/V1.0")           => true,
            _ => false
        };
        
        if hipcam && path != "/11" && path != "/12" {
            Ok(DescribeStatus::NotFound)
        } else {
            match header.code {
                404 => Ok(DescribeStatus::NotFound),
                401 => Ok(DescribeStatus::Locked),
                200 if is_supported_service(&response.body) => 
                    Ok(DescribeStatus::Ok),
                200 => Ok(DescribeStatus::Unsupported),
                _   => Ok(DescribeStatus::Error)
            }
        }
    } else {
        Ok(DescribeStatus::Error)
    }
}

/// Find open ports on all available hosts within a given network and port 
/// range.
fn find_services(
    pc: pcap::ThreadingContext,
    device: &EthernetDevice,
    ports: &PortCollection) -> Result<ScanReport> {
    let mut report  = ScanReport::new();
    
    for (mac, ip) in try!(Ipv4ArpScanner::scan_device(pc.clone(), device)) {
        report.add_host(mac, IpAddr::V4(ip), HINFO_FLAG_ARP);
    }
    
    for (mac, ip) in try!(IcmpScanner::scan_device(pc.clone(), device)) {
        report.add_host(mac, IpAddr::V4(ip), HINFO_FLAG_ICMP);
    }
    
    let open_ports = {
        let hosts = report.hosts()
            .map(|host| (host.mac_addr, host.ip_addr));
        
        try!(find_open_ports(pc, device, hosts, ports))
    };
    
    for (mac, addr) in open_ports {
        report.add_port(mac, addr.ip(), addr.port());
    }
    
    Ok(report)
}

/// Check if any of given TCP ports is open on on any host from a given set.
fn find_open_ports<H: IntoIterator<Item=(MacAddr, IpAddr)>>(
    pc: pcap::ThreadingContext,
    device: &EthernetDevice,
    hosts: H, 
    ports: &PortCollection) -> Result<Vec<(MacAddr, SocketAddr)>> {
    let hosts = hosts.into_iter()
        .filter_map(|(mac, ip)| match ip {
            IpAddr::V4(ip) => Some((mac, ip)),
            _              => None
        });
    
    let res = try!(TcpPortScanner::scan_ipv4_hosts(pc, device, hosts, ports))
        .into_iter()
        .map(|(mac, ip, p)| (mac, SocketAddr::V4(SocketAddrV4::new(ip, p))))
        .collect::<Vec<_>>();
    
    Ok(res)
}

/// Find all RTSP services among a given set of sockets.
fn find_rtsp_services(
    report: &ScanReport) -> Result<Vec<(MacAddr, SocketAddr)>> {
    let mut threads = Vec::new();
    let mut res     = Vec::new();
    
    for (mac, addr) in report.socket_addrs() {
        let handle = thread::spawn(move || {
            (mac, addr, is_rtsp_service(addr))
        });
        threads.push(handle);
    }
    
    for handle in threads {
        if let Ok((mac, addr, rtsp)) = handle.join() {
            if try!(rtsp) {
                res.push((mac, addr));
            }
        } else {
            return Err(DiscoveryError::from("RTSP service testing thread panicked"));
        }
    }
    
    Ok(res)
}

/// Find all available RTSP paths for a given RTSP service.
fn find_rtsp_paths(
    mac: MacAddr, 
    addr: SocketAddr, 
    paths: &[String]) -> Result<Vec<Service>> {
    let mut ok          = Vec::new();
    let mut unsupported = Vec::new();
    let mut locked      = false;
    
    for path in paths {
        match try!(get_describe_status(addr, path)) {
            DescribeStatus::Ok          => ok.push(path.to_string()),
            DescribeStatus::Unsupported => unsupported.push(path.to_string()),
            DescribeStatus::Locked      => locked = true,
            _ => ()
        }
    }
    
    let mut res = ok.into_iter()
        .map(|path| Service::RTSP(mac, addr, path))
        .collect::<Vec<_>>();
    
    let unsupported = unsupported.into_iter()
        .map(|path| Service::UnsupportedRTSP(mac, addr, path))
        .collect::<Vec<_>>();
    
    res.extend(unsupported);
    
    // Some RTSP servers respond with RTSP 200 to all paths even though they 
    // cannot stream from all the paths. We should treat them as unknown RTSP 
    // services.
    if res.len() == paths.len() {
        res.clear();
    }
    
    if locked {
        res.push(Service::LockedRTSP(mac, addr));
    }
    
    if res.is_empty() {
        res.push(Service::UnknownRTSP(mac, addr));
    }
    
    Ok(res)
}
