//! A one-shot mDNS/DNS-SD query: "who offers this service?". Asked from an ordinary port,
//! so responders answer us directly (RFC 6762 §6.7) and nothing has to share port 5353 with
//! the system's own responder.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use anyhow::Result;

const MDNS: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(224, 0, 0, 251)), 5353);

/// One service instance found.
#[derive(Debug, Clone, Default)]
pub struct Instance {
    /// "Living Room TV._googlecast._tcp.local"
    pub name: String,
    pub host: String,
    pub port: u16,
    pub ip: Option<IpAddr>,
    pub txt: HashMap<String, String>,
}

/// Ask for `service` ("_googlecast._tcp.local") and collect the answers for `wait`.
pub fn browse(service: &str, wait: Duration) -> Result<Vec<Instance>> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    sock.set_read_timeout(Some(Duration::from_millis(200)))?;
    let _ = sock.set_multicast_ttl_v4(255);
    let query = query(service);
    let mut instances: HashMap<String, Instance> = HashMap::new();
    let mut hosts: HashMap<String, IpAddr> = HashMap::new();
    let end = Instant::now() + wait;
    let mut next_send = Instant::now();
    let mut buf = [0u8; 9000];
    while Instant::now() < end {
        if Instant::now() >= next_send {
            let _ = sock.send_to(&query, MDNS);
            next_send = Instant::now() + Duration::from_millis(800);
        }
        let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
        let Some(records) = parse(&buf[..n]) else { continue };
        for r in records {
            match r.data {
                Data::Ptr(target) if r.name.eq_ignore_ascii_case(service) => {
                    instances.entry(target.clone()).or_insert_with(|| Instance { name: target, ..Default::default() });
                }
                Data::Srv { port, target } => {
                    let i = instances.entry(r.name.clone()).or_insert_with(|| Instance { name: r.name.clone(), ..Default::default() });
                    i.port = port;
                    i.host = target;
                    if i.ip.is_none() {
                        i.ip = Some(from.ip());
                    }
                }
                Data::Txt(entries) => {
                    let i = instances.entry(r.name.clone()).or_insert_with(|| Instance { name: r.name.clone(), ..Default::default() });
                    for e in entries {
                        if let Some((k, v)) = e.split_once('=') {
                            i.txt.insert(k.to_ascii_lowercase(), v.to_string());
                        }
                    }
                }
                Data::A(ip) => {
                    hosts.insert(r.name.to_ascii_lowercase(), IpAddr::V4(ip));
                }
                _ => {}
            }
        }
    }
    let mut out: Vec<Instance> = instances.into_values().filter(|i| i.port != 0).collect();
    for i in &mut out {
        if let Some(ip) = hosts.get(&i.host.to_ascii_lowercase()) {
            i.ip = Some(*ip);
        }
    }
    Ok(out)
}

fn query(service: &str) -> Vec<u8> {
    let mut q = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0]; // id 0, flags 0, one question
    for label in service.trim_end_matches('.').split('.') {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0);
    q.extend_from_slice(&12u16.to_be_bytes()); // PTR
    q.extend_from_slice(&0x8001u16.to_be_bytes()); // IN, unicast response please
    q
}

#[derive(Debug)]
enum Data {
    Ptr(String),
    Srv { port: u16, target: String },
    Txt(Vec<String>),
    A(Ipv4Addr),
    Other,
}

#[derive(Debug)]
struct Record {
    name: String,
    data: Data,
}

/// Every resource record in a response (answers, authority, additional).
fn parse(msg: &[u8]) -> Option<Vec<Record>> {
    if msg.len() < 12 || msg[2] & 0x80 == 0 {
        return None; // not a response
    }
    let count = |i: usize| u16::from_be_bytes([msg[i], msg[i + 1]]) as usize;
    let (qd, records) = (count(4), count(6) + count(8) + count(10));
    let mut at = 12;
    for _ in 0..qd {
        at = name(msg, at)?.1 + 4;
    }
    let mut out = Vec::new();
    for _ in 0..records {
        let (n, next) = name(msg, at)?;
        if next + 10 > msg.len() {
            return None;
        }
        let kind = u16::from_be_bytes([msg[next], msg[next + 1]]);
        let len = u16::from_be_bytes([msg[next + 8], msg[next + 9]]) as usize;
        let start = next + 10;
        let data = msg.get(start..start + len)?;
        let d = match kind {
            12 => Data::Ptr(name(msg, start)?.0),
            33 if len >= 6 => Data::Srv { port: u16::from_be_bytes([data[4], data[5]]), target: name(msg, start + 6)?.0 },
            16 => {
                let mut v = Vec::new();
                let mut i = 0;
                while i < data.len() {
                    let l = data[i] as usize;
                    v.push(String::from_utf8_lossy(data.get(i + 1..i + 1 + l)?).into_owned());
                    i += 1 + l;
                }
                Data::Txt(v)
            }
            1 if len == 4 => Data::A(Ipv4Addr::new(data[0], data[1], data[2], data[3])),
            _ => Data::Other,
        };
        out.push(Record { name: n, data: d });
        at = start + len;
    }
    Some(out)
}

/// A (possibly compressed) name at `at`: the name and where the bytes after it begin.
fn name(msg: &[u8], mut at: usize) -> Option<(String, usize)> {
    let mut labels = Vec::new();
    let mut end = None;
    for _ in 0..64 {
        let len = *msg.get(at)? as usize;
        if len == 0 {
            return Some((labels.join("."), end.unwrap_or(at + 1)));
        }
        if len & 0xC0 == 0xC0 {
            let ptr = ((len & 0x3F) << 8) | *msg.get(at + 1)? as usize;
            end.get_or_insert(at + 2);
            at = ptr;
            continue;
        }
        labels.push(String::from_utf8_lossy(msg.get(at + 1..at + 1 + len)?).into_owned());
        at += 1 + len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name_bytes(n: &str) -> Vec<u8> {
        let mut v = Vec::new();
        for l in n.split('.') {
            v.push(l.len() as u8);
            v.extend_from_slice(l.as_bytes());
        }
        v.push(0);
        v
    }

    #[test]
    fn a_cast_answer_parses() {
        // Header: response, 0 questions, 1 answer, 0, 3 additional.
        let mut m = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 3];
        let service = 12;
        m.extend(name_bytes("_googlecast._tcp.local"));
        m.extend([0, 12, 0, 1, 0, 0, 0, 120]);
        let inst = name_bytes("Living-Room-TV-abc");
        m.extend(((inst.len() + 1) as u16).to_be_bytes());
        m.extend(&inst[..inst.len() - 1]);
        m.extend([0xC0, service as u8]); // + "._googlecast._tcp.local"
        let inst_at = m.len() - inst.len() - 1;
        // SRV for the instance, by pointer.
        m.extend([0xC0, inst_at as u8, 0, 33, 0x80, 1, 0, 0, 0, 120]);
        let host = name_bytes("abc.local");
        m.extend(((6 + host.len()) as u16).to_be_bytes());
        m.extend([0, 0, 0, 0, 0x1F, 0x49]); // port 8009
        let host_at = m.len();
        m.extend(&host);
        // TXT
        m.extend([0xC0, inst_at as u8, 0, 16, 0x80, 1, 0, 0, 0, 120]);
        let txt: Vec<u8> = ["id=abc", "fn=Living Room TV", "md=SHIELD Android TV"]
            .iter()
            .flat_map(|s| std::iter::once(s.len() as u8).chain(s.bytes()))
            .collect();
        m.extend((txt.len() as u16).to_be_bytes());
        m.extend(&txt);
        // A
        m.extend([0xC0, host_at as u8, 0, 1, 0x80, 1, 0, 0, 0, 120, 0, 4, 192, 168, 0, 190]);

        let r = parse(&m).unwrap();
        assert_eq!(r.len(), 4);
        assert!(matches!(&r[0].data, Data::Ptr(t) if t == "Living-Room-TV-abc._googlecast._tcp.local"));
        assert!(matches!(&r[1].data, Data::Srv { port: 8009, target } if target == "abc.local"));
        assert!(matches!(&r[2].data, Data::Txt(v) if v[1] == "fn=Living Room TV"));
        assert!(matches!(r[3].data, Data::A(ip) if ip == Ipv4Addr::new(192, 168, 0, 190)));
    }
}
