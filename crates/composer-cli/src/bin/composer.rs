#[cfg(all(feature = "mimalloc", not(debug_assertions)))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    std::process::exit(composer_rs_cli::run());
}
