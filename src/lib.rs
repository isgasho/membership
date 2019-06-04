use mio::*;
use mio::net::*;
use structopt::StructOpt;
use std::net::{IpAddr, SocketAddr, Ipv4Addr, SocketAddrV4};
use std::str::FromStr;
use std::time::Duration;
use bytes::{BufMut, Buf, BytesMut};
use std::io::Cursor;
use std::collections::vec_deque::VecDeque;
use std::collections::{HashMap, HashSet, BTreeMap};
use std::fmt;
use log::{debug, info, error};
use std::default::Default;

mod message;
use crate::message::{MessageType, Message};

#[derive(StructOpt, Default)]
pub struct ProtocolConfig {
    #[structopt(short="p", long="port", default_value="2345")]
    port: u16,

    #[structopt(short="b", long="bind-address", default_value="127.0.0.1")]
    bind_address: String,

    #[structopt(short="o", long="proto-period", default_value="5")]
    protocol_period: u64,

    #[structopt(short="a", long="ack-timeout", default_value="1")]
    ack_timeout: u8,
}

#[derive(StructOpt, Default)]
pub struct Config {
    #[structopt(short="j", long="join-address", default_value="127.0.0.1")]
    pub join_address: String,

    // FIXME: this should not be public, fix dependencies between the two configs, make clear which is about protocol
    // and which about client properties.
    #[structopt(flatten)]
    pub proto_config: ProtocolConfig,
}

struct OutgoingLetter {
    target: SocketAddr,
    message: message::Message,
}

impl fmt::Debug for OutgoingLetter {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "OutgoingLetter {{ target: {}, message: {:?} }}", self.target, self.message)
    }
}

struct IncomingLetter {
    sender: SocketAddr,
    message: message::Message,
}

impl fmt::Debug for IncomingLetter {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "IncomingLetter {{ sender: {}, message: {:?} }}", self.sender, self.message)
    }
}

pub struct Gossip {
    config: ProtocolConfig,
    server: Option<UdpSocket>,
    members: Vec<SocketAddr>,
    members_presence: HashSet<SocketAddr>,
    next_member_index: usize,
    epoch: u64,
    recv_buffer: [u8; 64],
    myself: SocketAddr,
}

//impl Default for Gossip {
//    fn default() -> Self {
//        let config_default = Default::default();
//        Gossip {
//            config: config_default,
//            client: None,
//            server: None,
//            timer: Default::default(),
//            members: Default::default(),
//            members_presence: Default::default(),
//            next_member_index: 0,
//            epoch: 0,
//            recv_buffer: [0; 64],
//            myself: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from_str(&config_default.bind_address).unwrap(), config_default.port))
//        }
//    }
//}

impl Gossip {
    pub fn new(config: ProtocolConfig) -> Gossip {
        let myself = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from_str(&config.bind_address).unwrap(), config.port));
        Gossip{
            config,
            server: None,
            members: vec!(),
            members_presence: HashSet::new(),
            next_member_index: 0,
            epoch: 0,
            recv_buffer: [0; 64],
            myself
        }
    }

    pub fn join(&mut self, _member: IpAddr) {
        self.join_members(std::iter::once(SocketAddr::new(_member, self.config.port)));
        let poll = Poll::new().unwrap();
        self.bind(&poll);

        let mut events = Events::with_capacity(1024);
        let mut sequence_number: u64 = 0;
        let mut pings_to_confirm = VecDeque::with_capacity(1024);
        let mut wait_ack: HashMap<u64, SocketAddr> = HashMap::new();
        let mut ack_timeouts: BTreeMap<std::time::Instant, u64> = BTreeMap::new();
        let mut last_epoch_time = std::time::Instant::now();
        loop {
            poll.poll(&mut events, Some(Duration::from_millis(100))).unwrap();
            for event in events.iter() {
                debug!("{:?}", event);
                if event.readiness().is_readable() {
                    match event.token() {
                        Token(53) => {
                            let letter = self.recv_letter();
                            self.join_members(
                                letter.message.get_members().into_iter().chain(std::iter::once(letter.sender))
                            );
                            match letter.message.get_type() {
                                // FIXME: even when switching epochs it should not pause responding to Pings
                                message::MessageType::Ping => {
                                    pings_to_confirm.push_back(letter);
                                    poll.reregister(self.server.as_ref().unwrap(), Token(53), Ready::readable()|Ready::writable(), PollOpt::edge()).unwrap();
                                }
                                message::MessageType::PingAck => {
                                    // check the key is in `wait_ack`, if it is not the node might have already be marked as failed
                                    // and removed from the cluster
                                    wait_ack.remove(&letter.message.get_sequence_number());
                                }
                                message::MessageType::PingIndirect => {

                                }
                            }
                        }
                        _ => unreachable!()
                    }
                } else if event.readiness().is_writable() {
                    match event.token() {
                        Token(43) => {
                            if self.members.len() > 0 {
                                let target = self.members[self.next_member_index];
//                                self.send_ping(target, sequence_number);
                                let mut message = message::Message::create(message::MessageType::Ping, sequence_number, self.epoch);
                                // FIXME pick members with the lowest recently visited counter (mark to not starve the ones with highest visited counter)
                                // as that may lead to late failure discovery
                                message.with_members(
                                    &self.members.iter().skip(self.next_member_index).chain(self.members.iter().take(self.next_member_index)).cloned().collect::<Vec<_>>()
                                );
                                self.send_letter(OutgoingLetter { message, target });
                                wait_ack.insert(sequence_number, target);
                                ack_timeouts.insert(std::time::Instant::now(), sequence_number);
                                sequence_number += 1;
                                self.next_member_index = (self.next_member_index + 1) % self.members.len();
                            }
                            poll.reregister(self.server.as_ref().unwrap(), Token(53), Ready::readable()|Ready::writable(), PollOpt::edge()).unwrap();
                        }
                        Token(53) => {
                            // TODO: confirm pings until WouldBlock
                            if let Some(confirm) = pings_to_confirm.pop_front() {
                                let mut message = message::Message::create(message::MessageType::PingAck, confirm.message.get_sequence_number(), confirm.message.get_epoch());
                                message.with_members(self.members.as_slice());
                                let letter = OutgoingLetter { message, target: confirm.sender };
                                self.send_letter(letter);
                            } else {
                                poll.reregister(self.server.as_ref().unwrap(), Token(53), Ready::readable(), PollOpt::edge()).unwrap();
                            }
                        }
                        Token(63) => {
                            // TODO: PingIndirect
                        }
                        _ => unreachable!()
                    }
                }
            }
            let now = std::time::Instant::now();
            if now > (last_epoch_time + Duration::from_secs(self.config.protocol_period)) {
                self.epoch += 1;
                // TODO: mark the nodes as suspected first.
                self.remove_members(wait_ack.drain().map(|(_, sa)|{sa}));
                poll.reregister(self.server.as_ref().unwrap(), Token(43), Ready::writable(), PollOpt::edge()).unwrap();
                last_epoch_time = now;
                info!("New epoch: {}", self.epoch);
            }

            let mut drained: BTreeMap<std::time::Instant, u64>;
            let (drained, new_timeouts): (BTreeMap<std::time::Instant, u64>, BTreeMap<std::time::Instant, u64>) = ack_timeouts.into_iter().partition(|(t, _)| {now > (*t + Duration::from_secs(self.config.ack_timeout as u64))});
            for (time, sequence_number) in drained {
                if let Some(removed) = wait_ack.remove(&sequence_number) {
                    // ping indirect
                    poll.reregister(self.server.as_ref().unwrap(), Token(63), Ready::writable(), PollOpt::edge()).unwrap();
//                    self.remove_members(std::iter::once(removed));
                }
            }
            ack_timeouts = new_timeouts;
//            self.check_ack();
        }
    }

//    fn send_ping(&mut self, target: SocketAddr, sequence_number: u64) {
//        let mut message = Message::create(MessageType::Ping, sequence_number, self.epoch);
//        // FIXME pick members with the lowest recently visited counter (mark to not starve the ones with highest visited counter)
//        // as that may lead to late failure discovery
//        let member_index = self.members.iter().position(|x| { *x == target}).unwrap();
//        message.with_members(
//            &self.members.iter().skip(member_index).chain(self.members.iter().take(member_index)).cloned().collect::<Vec<_>>()
//        );
//        self.send_letter(OutgoingLetter { message, target });
////        wait_ack.insert(sequence_number, target);
//    }

    fn bind(&mut self, poll: &Poll) {
        let address = format!("{}:{}", self.config.bind_address, self.config.port).parse().unwrap();
        self.server = Some(UdpSocket::bind(&address).unwrap());
        poll.register(self.server.as_ref().unwrap(), Token(43), Ready::readable() | Ready::writable(), PollOpt::edge()).unwrap();
    }

    fn send_letter(&mut self, letter: OutgoingLetter) {
        debug!("{:?}", letter);
        self.server.as_ref().unwrap().send_to(&letter.message.into_inner(), &letter.target).unwrap();
    }

    fn recv_letter(&mut self) -> IncomingLetter {
        let (_, sender) = self.server.as_ref().unwrap().recv_from(&mut self.recv_buffer).unwrap();
        let letter = IncomingLetter{sender, message: message::Message::from(&self.recv_buffer)};
        debug!("{:?}", letter);
        letter
    }

    fn join_members<T>(&mut self, members: T) where T: Iterator<Item = SocketAddr> {
        for member in members {
            if member == self.myself {
                continue;
            }
            if self.members_presence.insert(member) {
                info!("Member joined: {:?}", member);
                self.members.push(member);
            }
        }
    }

    fn remove_members<T>(&mut self, members: T) where T: Iterator<Item = SocketAddr> {
        for member in members {
            if self.members_presence.remove(&member) {
                let idx = self.members.iter().position(|e| { *e == member }).unwrap();
                self.members.remove(idx);
                if idx <= self.next_member_index && self.next_member_index > 0 {
                    self.next_member_index -= 1;
                }
                info!("Member removed: {:?}", member);
            }
        }
    }

    fn increment_epoch(&mut self) {
        //
    }

    fn ping_indirect(&mut self) {

    }

    fn check_ack(&mut self) {

    }
}

