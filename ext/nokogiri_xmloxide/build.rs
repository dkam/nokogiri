// Re-emits rb-sys's link arguments for this cdylib. On macOS that includes
// `-Wl,-undefined,dynamic_lookup`, so Ruby's symbols resolve at load time
// from the running interpreter instead of failing at link time.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    rb_sys_env::activate()?;
    Ok(())
}
