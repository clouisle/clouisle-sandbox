//! clouisle-images: OCI 镜像拉取与 rootfs ext4 构建管道（FR-06）。

pub mod builder;
pub mod volumes;

pub use builder::ImageSpec;
pub use volumes::{Volume, VolumeError};