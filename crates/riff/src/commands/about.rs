use anyhow::Result;

#[derive(usage_rs::Args, Debug)]
pub struct AboutArgs {}

pub fn execute(_args: AboutArgs) -> Result<i32> {
    riff_core::outln!(
        "Riff - Composer-compatible Dependency Manager for PHP - version {}",
        env!("CARGO_PKG_VERSION")
    );
    riff_core::outln!(
        "Riff is a fast, standalone package manager tracking local dependencies of your projects and libraries."
    );
    riff_core::outln!("See https://github.com/shyim/riff for more information.");
    Ok(0)
}
