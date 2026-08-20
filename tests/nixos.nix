{ pkgs, package }:

pkgs.testers.runNixOSTest {
  name = "airman-daemon-integration";

  nodes.machine = { ... }: {
    networking.networkmanager.enable = true;
    hardware.bluetooth.enable = true;
    boot.kernelModules = [
      "bluetooth"
      "hci_vhci"
    ];

    environment.systemPackages = [
      package
      pkgs.jq
    ];
  };

  testScript = ''
    start_all()
    machine.wait_for_unit("dbus.service")
    machine.wait_for_unit("NetworkManager.service")
    machine.succeed("systemctl start bluetooth.service")
    machine.wait_until_succeeds(
      "busctl --system status org.freedesktop.NetworkManager", timeout=60
    )
    machine.wait_until_succeeds(
      "busctl --system status org.bluez", timeout=60
    )

    machine.succeed(
      "airman --backend network-manager --log-file /tmp/airman.log diagnose --json > /tmp/diagnostics.json"
    )
    machine.succeed(
      "jq -e '.backends[] | select(.backend == \"NetworkManager\" and .available == true)' /tmp/diagnostics.json"
    )
    machine.succeed(
      "jq -e '.backends[] | select(.backend == \"BlueZ\" and .available == true)' /tmp/diagnostics.json"
    )
    machine.succeed("test -s /tmp/airman.log")
  '';
}
