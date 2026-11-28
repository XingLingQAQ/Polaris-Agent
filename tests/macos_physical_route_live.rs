// Read-only integration check; requires a macOS host with a TUN default and
// a usable physical scoped default. No Agent config, tokens or services used.
#[cfg(target_os = "macos")]
#[path = "../src/sysstat/network_route.rs"]
mod network_route;

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires a live macOS TUN default plus physical scoped default"]
fn live_tun_default_falls_back_to_physical_address() {
    let route = std::process::Command::new("/sbin/route")
        .args(["-n", "get", "default"])
        .output()
        .unwrap();
    assert!(route.status.success());
    let text = String::from_utf8(route.stdout).unwrap();
    let interface = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("interface:").map(str::trim))
        .unwrap();
    assert!(
        interface.strip_prefix("utun").is_some_and(|index| {
            !index.is_empty() && index.bytes().all(|c| c.is_ascii_digit())
        }),
        "live precondition: default must be TUN"
    );
    let selected = network_route::physical_address();
    let expected: std::net::IpAddr = std::env::var("POLARIS_EXPECTED_PHYSICAL_IPV4")
        .expect("set expected physical address from independent ipconfig check")
        .parse()
        .unwrap();
    assert!(
        selected == Some(expected),
        "physical address must match the independently checked interface"
    );
}
