//! Generated gRPC code lives here, included via tonic's build-time codegen.
//! agent and fs both depend on this crate to talk to each other —
//! neither hand-writes any wire format logic.

pub mod fs {
    pub mod v1 {
        tonic::include_proto!("nexus.fs.v1");
    }
}
