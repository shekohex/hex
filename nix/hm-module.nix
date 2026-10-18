{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.hex;
  jsonFormat = pkgs.formats.json { };
  initialSettingsFile = jsonFormat.generate "hex-initial-settings.json" cfg.initialSettings;
  waitForSession = pkgs.writeShellApplication {
    name = "hex-wait-for-session";
    runtimeInputs = [
      pkgs.coreutils
      pkgs.gnugrep
      pkgs.xprop
    ];
    text = builtins.readFile ./wait-for-session.sh;
  };
in
{
  options.programs.hex = {
    enable = lib.mkEnableOption "HEX local voice dictation";
    package = lib.mkOption {
      type = lib.types.package;
      description = "HEX package to install.";
    };
    autostart = lib.mkEnableOption "HEX startup with the graphical session";
    gnomeIntegration = lib.mkEnableOption "the HEX GNOME Shell integration (GNOME 49/50)";
    initialSettings = lib.mkOption {
      type = lib.types.attrsOf jsonFormat.type;
      default = { };
      example = {
        dictation_hotkey = {
          alt = false;
          super_key = true;
          key = "space";
        };
        sound_effect_volume = 0.25;
      };
      description = ''
        Initial Linux runtime settings, merged with fresh-install defaults and
        saved only when linux-settings.json does not exist. Later app edits and
        existing settings take precedence, including after Home Manager switches.
        Use environment variable names or private key-file paths for credentials;
        these defaults are public in the Nix store.
      '';
    };
    systemdTarget = lib.mkOption {
      type = lib.types.str;
      default = "graphical-session.target";
      example = "sway-session.target";
      description = ''
        User target that owns the graphical session. The compositor or session
        manager must import its display environment before starting this target,
        and stop the target at logout. This module does not start a session.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    xdg.dataFile = lib.mkIf cfg.gnomeIntegration {
      "gnome-shell/extensions/hex@shekohex.dev".source =
        "${cfg.package}/share/gnome-shell/extensions/hex@shekohex.dev";
    };
    dconf.settings = lib.mkIf cfg.gnomeIntegration {
      "org/gnome/shell".enabled-extensions = lib.mkAfter [ "hex@shekohex.dev" ];
    };
    assertions = [
      {
        assertion = pkgs.stdenv.hostPlatform.system == "x86_64-linux";
        message = "The HEX Nix package supports x86_64-linux only.";
      }
    ];
    home.packages = [ cfg.package ];
    systemd.user.services.hex = {
      Unit = {
        Description = "HEX local voice dictation";
        After = [ cfg.systemdTarget ];
        PartOf = [ cfg.systemdTarget ];
      };
      Service = {
        ExecStartPre = lib.getExe waitForSession;
        ExecStart = "${lib.getExe cfg.package} service";
        Environment = [
          "HEX_APPLICATION_SUPPORT_DIR=${config.xdg.dataHome}/voice-control"
        ]
        ++ lib.optional (cfg.initialSettings != { }) "HEX_INITIAL_SETTINGS_FILE=${initialSettingsFile}";
        EnvironmentFile = "-${config.xdg.dataHome}/voice-control/session.env";
        TimeoutStartSec = 35;
        Restart = "on-failure";
        RestartSec = 5;
        TimeoutStopSec = 30;
        UMask = "0077";
      };
      Install.WantedBy = lib.optionals cfg.autostart [ cfg.systemdTarget ];
    };
  };
}
