# A small project devshell for the OpenShell learning tests: its closure is
# built on the workstation and baked into the sandbox image.
{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.05";

  outputs =
    { nixpkgs, ... }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
      ];
    in
    {
      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = [
              pkgs.jq
              pkgs.ripgrep
            ];
            shellHook = ''
              export WM_LEARN_DEVSHELL=1
            '';
          };
        }
      );
    };
}
