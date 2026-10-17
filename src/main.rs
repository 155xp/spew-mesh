fn main() -> std::process::ExitCode {
    spew_mesh::install_crypto_provider();
    spew_mesh::cli::main()
}
