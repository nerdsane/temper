//! Development server command for `temper serve`.
//!
//! Placeholder implementation. A full OData-compliant development server
//! backed by temper-server will be integrated in a future release.

use anyhow::Result;

/// Run the `temper serve` command.
///
/// Starts a development server on the specified port. This is currently a
/// placeholder that prints configuration information. The full implementation
/// will integrate with temper-server to provide an OData-compliant HTTP API
/// backed by the generated entity actors.
pub async fn run(port: u16) -> Result<()> {
    println!("Starting Temper development server...");
    println!("  Port: {port}");
    println!();
    println!("Note: The development server is not yet implemented.");
    println!("      This will be available once temper-server is integrated.");
    println!();
    println!("Planned features:");
    println!("  - OData v4 compliant REST API");
    println!("  - Hot reload on spec changes");
    println!("  - Built-in actor system with state machines");
    println!("  - Cedar policy enforcement");
    println!("  - OpenTelemetry tracing");

    Ok(())
}
