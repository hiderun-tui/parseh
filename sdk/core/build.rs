// Generates UniFFI bindings at build time.
// Produces the Kotlin / Swift binding files consumed by the mobile clients.

fn main() {
    uniffi::generate_scaffolding("./src/parseh.udl").expect("UniFFI scaffolding generation failed");
}
