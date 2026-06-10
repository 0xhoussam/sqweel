{
  description = "A Nix-flake-based Rust development environment";

  inputs = {
    nixpkgs.url = "https://flakehub.com/f/NixOS/nixpkgs/0.1"; # unstable Nixpkgs
    fenix = {
      url = "https://flakehub.com/f/nix-community/fenix/0.1";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { self, ... }@inputs:

    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forEachSupportedSystem =
        f:
        inputs.nixpkgs.lib.genAttrs supportedSystems (
          system:
          f {
            inherit system;
            pkgs = import inputs.nixpkgs {
              inherit system;
              overlays = [
                inputs.self.overlays.default
              ];
            };
          }
        );
    in
    {
      overlays.default = final: prev: {
        rustToolchain =
          with inputs.fenix.packages.${prev.stdenv.hostPlatform.system};
          combine (
            with stable;
            [
              clippy
              rustc
              cargo
              rustfmt
              rust-src
            ]
          );
      };

      devShells = forEachSupportedSystem (
        { pkgs, system }:
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              rustToolchain
              openssl
              pkg-config
              cargo-deny
              cargo-edit
              cargo-watch
              rust-analyzer
              self.formatter.${system}

              # GTK4 + libadwaita + Blueprint
              gtk4
              libadwaita
              gtksourceview5
              blueprint-compiler
              glib
              gobject-introspection
              graphene
              cairo
              pango
              gdk-pixbuf
              gsettings-desktop-schemas
              adwaita-icon-theme
              wrapGAppsHook4

              # libdbus for keyring (secret service)
              dbus
            ];

            env = {
              # Required by rust-analyzer
              RUST_SRC_PATH = "${pkgs.rustToolchain}/lib/rustlib/src/rust/library";

              # Runtime lookup for GTK/Adwaita shared objects (cargo doesn't rpath nix store).
              LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
                pkgs.gtk4
                pkgs.libadwaita
                pkgs.gtksourceview5
                pkgs.glib
                pkgs.graphene
                pkgs.cairo
                pkgs.pango
                pkgs.gdk-pixbuf
                pkgs.gobject-introspection
                pkgs.dbus
              ];
            };

            # GSettings schemas (color-scheme, etc.) and icon themes live in nix
            # store paths GTK won't find on its own. wrapGAppsHook4 only fixes
            # packaged builds, so wire them up for `cargo run` in the dev shell.
            shellHook = ''
              export XDG_DATA_DIRS="${pkgs.gtk4}/share/gsettings-schemas/${pkgs.gtk4.name}:${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}:${pkgs.glib}/share/gsettings-schemas/${pkgs.glib.name}:${pkgs.gtksourceview5}/share:${pkgs.adwaita-icon-theme}/share:${pkgs.hicolor-icon-theme}/share:$XDG_DATA_DIRS"
            '';
          };
        }
      );

      formatter = forEachSupportedSystem ({ pkgs, ... }: pkgs.nixfmt);
    };
}
