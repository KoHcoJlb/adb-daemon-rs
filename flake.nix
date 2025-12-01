{
  inputs = {
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    { crane, ... }:

    let
      mkPackages =
        pkgs:
        with pkgs.lib;
        let
          craneLib = crane.mkLib pkgs;
        in
        {
          default = pkgs.callPackage (
            { rust, stdenv }:

            craneLib.buildPackage (
              {
                src = cleanSourceWith {
                  filter = craneLib.filterCargoSources;
                  src = ./.;
                  name = "source";
                };
              }
              // (
                with rust.envVars;
                with stdenv;
                {
                  "CARGO_BUILD_TARGET" = rustHostPlatform;

                  "CC_${stdenv.hostPlatform.rust.cargoEnvVarTarget}" = ccForHost;
                  "CXX_${stdenv.hostPlatform.rust.cargoEnvVarTarget}" = cxxForHost;
                  "CARGO_TARGET_${stdenv.hostPlatform.rust.cargoEnvVarTarget}_LINKER" = ccForHost;
                }
                // optionalAttrs (stdenv.hostPlatform != stdenv.buildPlatform) {
                  "CC_${stdenv.buildPlatform.rust.cargoEnvVarTarget}" = ccForBuild;
                  "CXX_${stdenv.buildPlatform.rust.cargoEnvVarTarget}" = cxxForBuild;
                  "CARGO_TARGET_${stdenv.buildPlatform.rust.cargoEnvVarTarget}_LINKER" = ccForBuild;
                  "HOST_CC" = ccForBuild;
                  "HOST_CXX" = cxxForBuild;
                }
              )
            )
          ) { };
        };

    in
    {
      overlays.default = final: prev: {
        adb-daemon-rs = (mkPackages final).default;
      };
    };
}
