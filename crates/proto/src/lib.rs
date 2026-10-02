// Generated tonic/prost code: newer clippy (1.99) flags `#[must_use]` on the
// `async_trait`-expanded service traits (`double_must_use`). We do not control
// that output, so allow the lint for this crate only.
#![allow(clippy::double_must_use)]

pub mod fs {
    pub mod v1 {
        tonic::include_proto!("nexus.fs.v1");
    }
}

pub mod migrate {
    pub mod v1 {
        tonic::include_proto!("nexus.migrate.v1");
    }
}

pub mod pair {
    pub mod v1 {
        tonic::include_proto!("nexus.pair.v1");
    }
}

pub mod stream {
    pub mod v1 {
        tonic::include_proto!("nexus.stream.v1");
    }
}
