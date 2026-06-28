pub mod condition;
pub mod headscale_instance;

pub use condition::ResourceStatus;
pub use headscale_instance::{
    HeadscaleInstance, HeadscaleInstancePolicy, HeadscaleInstanceSpec, HeadscaleInstanceStatus,
    ScimSpec, StorageSpec,
};
