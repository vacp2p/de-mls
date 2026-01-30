// Core library
pub mod core;

// Example application layer (reference implementation)
pub mod app;

pub mod protos {
    pub mod hashgraph_like_consensus {
        pub mod v1 {
            pub use ::hashgraph_like_consensus::protos::consensus::v1::*;
        }
    }

    pub mod de_mls {
        pub mod messages {
            pub mod v1 {
                include!(concat!(env!("OUT_DIR"), "/de_mls.messages.v1.rs"));
            }
        }
    }
}
