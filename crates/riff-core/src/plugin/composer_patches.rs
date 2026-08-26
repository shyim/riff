use super::manager::{PluginDescriptor, PluginRegistrar};

const PACKAGE_NAME: &str = "cweagans/composer-patches";

pub(super) fn register(registrar: &mut PluginRegistrar) {
    // Patching is a first-class Riff feature. Registering this compatibility
    // descriptor only tells policy validation that the Composer plugin package
    // has a native implementation and does not need to execute PHP code.
    registrar.descriptor(PluginDescriptor::new(PACKAGE_NAME));
}
