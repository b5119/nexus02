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
