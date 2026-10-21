//! PC to PC (a controlling PC and a receiver PC): discovery, pairing and session messages on the
//! receiver's `CONTROL_PORT`, and `paired.ini`. Pure; the crypto is in `pair.rs`.
//!
//! The controlling PC broadcasts a `Query` every 2 s; receivers answer with a `Beacon` (unicast,
//! so the controlling PC needs no inbound firewall rule). Pairing (numeric comparison, see
//! `pair.rs`): `PairRequest` -> `Commit` -> `Nonce` -> `Reveal`, both screens show the code, each
//! side sends `Confirm` once its user says yes. A session: `Hello` carries a fresh session key and
//! the side, sealed with the paired key and bound to the beacon's current nonce; then the usual
//! PcState/TabState datagrams (`proto::PORT`) and clipboard frames (`proto::CLIP_PORT`) follow,
//! as with the Android server.

use wadisplay_proto::{hex, open_bytes, seal_bytes, unhex, Side};

pub const CONTROL_PORT: u16 = 27186;
const MAGIC: &[u8; 4] = b"WAD1";
/// Directions under a paired key (session keys use proto's 0..=3).
pub const DIR_HELLO: u8 = 4;
pub const DIR_CONFIRM_PC: u8 = 5;
pub const DIR_CONFIRM_RX: u8 = 6;

pub type Id = [u8; 16];
/// A P-256 public key, X || Y.
pub type Public = [u8; 64];
/// Ties pairing messages to one attempt: the start of the controlling PC's public key.
pub type Tag = [u8; 8];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    Query,
    Beacon { id: Id, nonce: [u8; 16], public: Public, name: String },
    PairRequest { id: Id, public: Public, name: String },
    Commit { tag: Tag, commit: [u8; 32] },
    Nonce { tag: Tag, nonce: [u8; 16] },
    Reveal { tag: Tag, nonce: [u8; 16] },
    Confirm { tag: Tag, sealed: Vec<u8> },
    Hello { id: Id, sealed: Vec<u8> },
}

/// Names are cut to this many bytes (at a character boundary).
const MAX_NAME: usize = 64;

fn name_bytes(name: &str) -> Vec<u8> {
    let mut n = name.len().min(MAX_NAME);
    while !name.is_char_boundary(n) {
        n -= 1;
    }
    [&[n as u8][..], &name.as_bytes()[..n]].concat()
}

impl Link {
    pub fn encode(&self) -> Vec<u8> {
        let (kind, body): (u8, Vec<u8>) = match self {
            Link::Query => (b'Q', vec![]),
            Link::Beacon { id, nonce, public, name } => (b'B', [&id[..], nonce, public, &name_bytes(name)].concat()),
            Link::PairRequest { id, public, name } => (b'R', [&id[..], public, &name_bytes(name)].concat()),
            Link::Commit { tag, commit } => (b'C', [&tag[..], commit].concat()),
            Link::Nonce { tag, nonce } => (b'N', [&tag[..], nonce].concat()),
            Link::Reveal { tag, nonce } => (b'V', [&tag[..], nonce].concat()),
            Link::Confirm { tag, sealed } => (b'K', [&tag[..], sealed].concat()),
            Link::Hello { id, sealed } => (b'H', [&id[..], sealed].concat()),
        };
        [&MAGIC[..], &[kind], &body].concat()
    }

    pub fn decode(b: &[u8]) -> Option<Link> {
        let (kind, b) = b.strip_prefix(MAGIC)?.split_first()?;
        let arr = |b: &[u8], at: usize| -> Option<[u8; 16]> { b.get(at..at + 16)?.try_into().ok() };
        let public = |b: &[u8], at: usize| -> Option<Public> { b.get(at..at + 64)?.try_into().ok() };
        let tag = |b: &[u8]| -> Option<Tag> { b.get(..8)?.try_into().ok() };
        let name = |b: &[u8]| -> Option<String> {
            let (&n, rest) = b.split_first()?;
            (rest.len() == n as usize).then(|| String::from_utf8(rest.to_vec()).ok())?
        };
        Some(match kind {
            b'Q' if b.is_empty() => Link::Query,
            b'B' => Link::Beacon { id: arr(b, 0)?, nonce: arr(b, 16)?, public: public(b, 32)?, name: name(b.get(96..)?)? },
            b'R' => Link::PairRequest { id: arr(b, 0)?, public: public(b, 16)?, name: name(b.get(80..)?)? },
            b'C' if b.len() == 40 => Link::Commit { tag: tag(b)?, commit: b[8..].try_into().ok()? },
            b'N' if b.len() == 24 => Link::Nonce { tag: tag(b)?, nonce: arr(b, 8)? },
            b'V' if b.len() == 24 => Link::Reveal { tag: tag(b)?, nonce: arr(b, 8)? },
            b'K' if b.len() > 8 => Link::Confirm { tag: tag(b)?, sealed: b[8..].to_vec() },
            b'H' if b.len() > 16 => Link::Hello { id: arr(b, 0)?, sealed: b[16..].to_vec() },
            _ => return None,
        })
    }
}

pub fn tag(public: &Public) -> Tag {
    public[..8].try_into().unwrap()
}

/// What a `Hello` carries, sealed: the beacon nonce it answers, the session key, and where the
/// receiver sits and how far to push to return, as the Android server's start line has them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Session {
    pub nonce: [u8; 16],
    pub key: [u8; 32],
    pub side: Side,
    pub push_counts: i32,
}

impl Session {
    pub fn encode(&self) -> Vec<u8> {
        let side = [Side::Left, Side::Right, Side::Top, Side::Bottom].iter().position(|s| *s == self.side).unwrap() as u8;
        [&self.nonce[..], &self.key, &[side], &self.push_counts.to_le_bytes()].concat()
    }

    pub fn decode(b: &[u8]) -> Option<Session> {
        (b.len() == 53).then_some(())?;
        Some(Session {
            nonce: b[..16].try_into().ok()?,
            key: b[16..48].try_into().ok()?,
            side: *[Side::Left, Side::Right, Side::Top, Side::Bottom].get(b[48] as usize)?,
            push_counts: i32::from_le_bytes(b[49..53].try_into().ok()?),
        })
    }
}

/// A `Confirm`'s sealed answer, under the pairing key. Each answer has a sequence number of its own,
/// so retransmits repeat the same bytes and no nonce ever encrypts both answers.
pub fn confirm(key: &[u8; 32], dir: u8, yes: bool) -> Vec<u8> {
    seal_bytes(key, dir, if yes { 1 } else { 2 }, &[yes as u8])
}

pub fn confirmed(key: &[u8; 32], dir: u8, sealed: &[u8]) -> Option<bool> {
    let b = open_bytes(key, dir, sealed, &mut 0)?;
    match (&b[..], u64::from_le_bytes(sealed[..8].try_into().ok()?)) {
        ([1], 1) => Some(true),
        ([0], 2) => Some(false),
        _ => None,
    }
}

/// A PC this one paired with: on a controlling PC a receiver, on a receiver a controlling PC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paired {
    pub id: Id,
    pub name: String,
    pub key: [u8; 32],
}

pub const PAIRED_HEADER: &str = "\
# wadisplay: the PCs this one is paired with, and their keys (keep this file private). self is
# this PC's id. Delete a [peer] section to unpair; the other PC then has to pair again.
";

/// `paired.ini`: this PC's id, if set, and its peers.
pub fn parse_paired(text: &str) -> (Option<Id>, Vec<Paired>) {
    let (mut me, mut out, mut cur): (Option<Id>, Vec<Paired>, Option<(Option<Id>, String, Option<[u8; 32]>)>) = (None, vec![], None);
    let flush = |cur: &mut Option<(Option<Id>, String, Option<[u8; 32]>)>, out: &mut Vec<Paired>| {
        if let Some((Some(id), name, Some(key))) = cur.take() {
            out.push(Paired { id, name, key });
        }
    };
    for line in text.lines().map(str::trim).filter(|l| !l.starts_with('#')) {
        if line == "[peer]" {
            flush(&mut cur, &mut out);
            cur = Some((None, String::new(), None));
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        match (cur.as_mut(), k.trim()) {
            (None, "self") => me = unhex(v),
            (Some(c), "id") => c.0 = unhex(v),
            (Some(c), "name") => c.1 = v.into(),
            (Some(c), "key") => c.2 = unhex(v),
            _ => {}
        }
    }
    flush(&mut cur, &mut out);
    (me, out)
}

pub fn format_paired(me: &Id, peers: &[Paired]) -> String {
    let mut s = format!("{PAIRED_HEADER}self={}\n", hex(me));
    for p in peers {
        s += &format!("\n[peer]\nid={}\nname={}\nkey={}\n", hex(&p.id), p.name.replace(['\r', '\n'], " "), hex(&p.key));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        let all = [
            Link::Query,
            Link::Beacon { id: [1; 16], nonce: [2; 16], public: [3; 64], name: "书房 PC".into() },
            Link::PairRequest { id: [4; 16], public: [5; 64], name: String::new() },
            Link::Commit { tag: [6; 8], commit: [7; 32] },
            Link::Nonce { tag: [8; 8], nonce: [9; 16] },
            Link::Reveal { tag: [10; 8], nonce: [11; 16] },
            Link::Confirm { tag: [12; 8], sealed: vec![13; 30] },
            Link::Hello { id: [14; 16], sealed: vec![15; 77] },
        ];
        for m in all {
            let b = m.encode();
            assert_eq!(Link::decode(&b), Some(m.clone()));
            if !matches!(m, Link::Confirm { .. } | Link::Hello { .. }) {
                assert_eq!(Link::decode(&b[..b.len() - 1]), None, "short {m:?}");
            }
        }
        assert_eq!(Link::decode(b"WAD2Q"), None, "other magic");
        assert_eq!(Link::decode(b"WAD1Qx"), None);
        // A long name is cut at a character boundary.
        let b = Link::Beacon { id: [0; 16], nonce: [0; 16], public: [0; 64], name: "长".repeat(30) }.encode();
        let Some(Link::Beacon { name, .. }) = Link::decode(&b) else { panic!() };
        assert_eq!(name, "长".repeat(21));
    }

    #[test]
    fn session_round_trip() {
        let s = Session { nonce: [1; 16], key: [2; 32], side: Side::Bottom, push_counts: -7 };
        assert_eq!(Session::decode(&s.encode()), Some(s));
        assert_eq!(Session::decode(&s.encode()[1..]), None);
        let mut b = s.encode();
        b[48] = 4;
        assert_eq!(Session::decode(&b), None, "no such side");
    }

    #[test]
    fn confirm_answers_only_under_its_key_and_direction() {
        let k = [7; 32];
        assert_eq!(confirmed(&k, DIR_CONFIRM_PC, &confirm(&k, DIR_CONFIRM_PC, true)), Some(true));
        assert_eq!(confirmed(&k, DIR_CONFIRM_RX, &confirm(&k, DIR_CONFIRM_RX, false)), Some(false));
        assert_eq!(confirmed(&k, DIR_CONFIRM_RX, &confirm(&k, DIR_CONFIRM_PC, true)), None, "other direction");
        assert_eq!(confirmed(&[8; 32], DIR_CONFIRM_PC, &confirm(&k, DIR_CONFIRM_PC, true)), None, "other key");
        assert_eq!(confirmed(&k, DIR_CONFIRM_PC, &seal_bytes(&k, DIR_CONFIRM_PC, 1, &[0])), None, "a no at a yes's nonce");
    }

    #[test]
    fn paired_ini_round_trip() {
        let peers = vec![Paired { id: [1; 16], name: "Desk\nPC".into(), key: [2; 32] },
                         Paired { id: [3; 16], name: "客厅".into(), key: [4; 32] }];
        let (me, back) = parse_paired(&format_paired(&[9; 16], &peers));
        assert_eq!(me, Some([9; 16]));
        assert_eq!(back[0].name, "Desk PC", "one line");
        assert_eq!(back[1], peers[1]);
        assert_eq!(parse_paired("[peer]\nid=0101\nkey=02\n"), (None, vec![]), "bad hex: dropped");
    }
}
