// Use the launcher's packaging implementation without opening its UI.
fn main() -> anyhow::Result<()> {
    pyappify_lib::packaging::package(&std::env::args().skip(1).collect::<Vec<_>>())
}
