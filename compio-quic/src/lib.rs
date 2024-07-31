//! QUIC implementation for compio

#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![warn(missing_docs)]

mod connection;
mod endpoint;
mod socket;

pub use crate::{
    connection::{Connecting, Connection},
    endpoint::Endpoint,
};
