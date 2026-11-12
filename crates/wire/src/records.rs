//! Envelope records. Field numbers are permanent; see `docs/wire.md`.
//! API payloads (`Request.payload`, `Response.payload`) are records from
//! the `api` crate, encoded by method.

#[derive(Clone, PartialEq, prost::Message)]
pub struct Hello {
    #[prost(string, tag = "1")]
    pub build_family: String,
    #[prost(string, tag = "2")]
    pub software_version: String,
    /// `cli`, `gui`, `extension`, `bridge`.
    #[prost(string, tag = "3")]
    pub role: String,
    #[prost(string, repeated, tag = "4")]
    pub features: Vec<String>,
    #[prost(uint32, tag = "5")]
    pub max_frame: u32,
    #[prost(uint32, tag = "6")]
    pub max_message: u32,
    #[prost(uint32, tag = "7")]
    pub pid: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Welcome {
    #[prost(string, tag = "1")]
    pub build_family: String,
    #[prost(string, tag = "2")]
    pub software_version: String,
    #[prost(string, repeated, tag = "3")]
    pub features: Vec<String>,
    #[prost(uint32, tag = "4")]
    pub max_frame: u32,
    #[prost(uint32, tag = "5")]
    pub max_message: u32,
    /// Unique per daemon start.
    #[prost(string, tag = "6")]
    pub instance: String,
    #[prost(uint64, tag = "7")]
    pub epoch: u64,
    #[prost(string, tag = "8")]
    pub host: String,
    #[prost(uint32, tag = "9")]
    pub pid: u32,
    #[prost(string, tag = "10")]
    pub workspace: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum RejectReason {
    Unspecified = 0,
    IncompatibleProtocol = 1,
    IncompatibleBuild = 2,
    Refused = 3,
}

/// Sent instead of `Welcome`. Carries what the peer needs to explain the
/// mismatch and the way out (spec 11.4: "display a useful upgrade path").
#[derive(Clone, PartialEq, prost::Message)]
pub struct Reject {
    #[prost(enumeration = "RejectReason", tag = "1")]
    pub reason: i32,
    #[prost(string, tag = "2")]
    pub message: String,
    #[prost(string, tag = "3")]
    pub software_version: String,
    #[prost(string, tag = "4")]
    pub build_family: String,
    #[prost(uint32, tag = "5")]
    pub protocol_major: u32,
    #[prost(uint32, tag = "6")]
    pub protocol_minor: u32,
    #[prost(uint32, tag = "7")]
    pub pid: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Request {
    #[prost(uint64, tag = "1")]
    pub id: u64,
    #[prost(string, tag = "2")]
    pub method: String,
    #[prost(bytes = "vec", tag = "3")]
    pub payload: Vec<u8>,
    #[prost(uint32, tag = "4")]
    pub deadline_ms: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Response {
    #[prost(uint64, tag = "1")]
    pub id: u64,
    #[prost(bytes = "vec", tag = "2")]
    pub payload: Vec<u8>,
    /// Present when the call failed; `payload` is then empty.
    #[prost(int32, optional, tag = "3")]
    pub error_code: Option<i32>,
    #[prost(string, tag = "4")]
    pub error_message: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Event {
    #[prost(string, tag = "1")]
    pub topic: String,
    #[prost(uint64, tag = "2")]
    pub sequence: u64,
    #[prost(bytes = "vec", tag = "3")]
    pub payload: Vec<u8>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Bulk {
    #[prost(uint64, tag = "1")]
    pub stream: u64,
    #[prost(string, tag = "2")]
    pub topic: String,
    #[prost(bytes = "vec", tag = "3")]
    pub data: Vec<u8>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Goodbye {
    #[prost(string, tag = "1")]
    pub reason: String,
}
