use clap::Parser;
use gewu_cli::Cli;

#[test]
fn parses_canvas_loop_flags() {
    let cli = Cli::parse_from([
        "gewu-cli",
        "--canvas-loop",
        "--canvas-url",
        "http://127.0.0.1:3210",
        "--poll-interval-ms",
        "25",
    ]);

    assert!(cli.canvas_loop);
    assert_eq!(cli.canvas_url, "http://127.0.0.1:3210");
    assert_eq!(cli.poll_interval_ms, 25);
}
