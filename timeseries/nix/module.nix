# NixOS module: the archiver, and optionally the QuestDB it writes to.
#
# Both units are here because nixpkgs has no `services.questdb` -- only the
# package -- and a home server that wants three years of sensor history should
# not have to invent the database unit as well. `questdb.enable = false` leaves
# it out for a setup that already runs one somewhere else.
{ config, lib, pkgs, ... }:

let
  cfg = config.services.smarthome-timeseries;
  tomlFormat = pkgs.formats.toml { };

  # systemd puts credentials under this directory, and the settings file has to
  # name the path literally: it is generated at build time, where `%d` means
  # nothing.
  credentials = "/run/credentials/smarthome-timeseries.service";

  settings = lib.recursiveUpdate cfg.settings (
    lib.optionalAttrs (cfg.mqtt.passwordFile != null) {
      mqtt.password_file = "${credentials}/mqtt-password";
    }
    // lib.optionalAttrs (cfg.questdb.passwordFile != null) {
      questdb.password_file = "${credentials}/questdb-password";
    }
  );
  settingsFile = tomlFormat.generate "smarthome-timeseries.toml" settings;

  questdbConf = pkgs.writeText "questdb-server.conf" cfg.questdb.extraConfig;
in
{
  options.services.smarthome-timeseries = {
    enable = lib.mkEnableOption "the smart-home MQTT to QuestDB archiver and its dashboard";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ./package.nix { };
      defaultText = lib.literalExpression "pkgs.callPackage ./package.nix { }";
      description = "The archiver package to run.";
    };

    settings = lib.mkOption {
      inherit (tomlFormat) type;
      default = { };
      example = lib.literalExpression ''
        {
          mqtt.host = "127.0.0.1";
          questdb.retention = "3y";
          web.bind = "127.0.0.1:8087";
        }
      '';
      description = ''
        Contents of the settings file, as described in `timeseries/README.md`.
        Every field has a working default, so `{ }` archives a fleet that
        publishes to a broker and a QuestDB on localhost.

        Do not put passwords here: this file lands in the world-readable Nix
        store. Use {option}`mqtt.passwordFile` and
        {option}`questdb.passwordFile` instead.
      '';
    };

    mqtt.passwordFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = ''
        File holding the MQTT password, read by systemd as a credential and
        never copied into the Nix store.
      '';
    };

    questdb.passwordFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = "File holding the QuestDB password, as for MQTT.";
    };

    questdb.enable = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Run QuestDB on this machine as well. Turn it off to point the archiver
        at a database elsewhere -- then set `settings.questdb.url` to match.
      '';
    };

    questdb.package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.questdb;
      defaultText = lib.literalExpression "pkgs.questdb";
      description = ''
        QuestDB itself. Materialized views need 9.x; table TTL, which the
        retention setting relies on, has been available since 8.2.2.

        QuestDB's storage format only moves forward: a data directory written
        by one version cannot be opened by an older one, so a rollback of this
        package after an upgrade is not a rollback of the data.
      '';
    };

    questdb.httpEndpoint = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:9000";
      description = ''
        Where QuestDB answers HTTP: ILP ingest, SQL, and the web console. It is
        an option rather than a line in {option}`questdb.extraConfig` because
        three things need to agree on it -- the config file, the archiver's
        `questdb.url`, and anything that takes a checkpoint before a backup --
        and free text cannot be read back.

        Loopback by default, and it should stay there: none of those three
        interfaces authenticates anything.
      '';
    };

    questdb.dataDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/questdb";
      description = ''
        Where the database, its write-ahead log and its configuration live.
        This is the one directory worth backing up.
      '';
    };

    questdb.extraConfig = lib.mkOption {
      type = lib.types.lines;
      defaultText = lib.literalExpression ''
        '''
          http.bind.to=''${questdb.httpEndpoint}
          pg.net.bind.to=127.0.0.1:8812
          line.tcp.net.bind.to=127.0.0.1:9009
        '''
      '';
      description = ''
        Contents of QuestDB's `server.conf`, rewritten on every start.

        The defaults bind to loopback deliberately: QuestDB's HTTP interface
        serves ingestion, arbitrary SQL *and* the web console without
        authentication, so it has no business on the LAN of a house.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    # Derived rather than repeated: see `questdb.httpEndpoint`.
    services.smarthome-timeseries.questdb.extraConfig = lib.mkDefault ''
      http.bind.to=${cfg.questdb.httpEndpoint}
      pg.net.bind.to=127.0.0.1:8812
      line.tcp.net.bind.to=127.0.0.1:9009
    '';

    systemd.services.smarthome-timeseries = {
      description = "Smart-home MQTT to QuestDB archiver";
      wantedBy = [ "multi-user.target" ];
      # `after` only means the unit was started, not that the database is
      # answering -- so the service also waits for QuestDB itself, for up to
      # three minutes, rather than failing and being restarted into the same
      # race.
      after = [ "network-online.target" ] ++ lib.optional cfg.questdb.enable "questdb.service";
      wants = [ "network-online.target" ];

      serviceConfig = {
        ExecStart = "${lib.getExe cfg.package} ${settingsFile}";
        Restart = "always";
        RestartSec = 5;

        # No state of its own: everything it knows is in QuestDB or on the
        # broker, so it can run as a user that exists only while it does.
        DynamicUser = true;
        LoadCredential =
          lib.optional (cfg.mqtt.passwordFile != null) "mqtt-password:${cfg.mqtt.passwordFile}"
          ++ lib.optional (cfg.questdb.passwordFile != null)
            "questdb-password:${cfg.questdb.passwordFile}";

        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictAddressFamilies = [ "AF_INET" "AF_INET6" ];
        RestrictNamespaces = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        SystemCallArchitectures = "native";
        SystemCallFilter = [ "@system-service" "~@privileged" "~@resources" ];
      };
    };

    systemd.services.questdb = lib.mkIf cfg.questdb.enable {
      description = "QuestDB time-series database";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];

      # Rewritten rather than merged: the file belongs to this configuration,
      # and a leftover setting from a previous generation surviving in the data
      # directory would make the running database differ from what the
      # configuration says.
      preStart = ''
        mkdir -p ${cfg.questdb.dataDir}/conf
        install -m 0644 ${questdbConf} ${cfg.questdb.dataDir}/conf/server.conf
      '';

      serviceConfig = {
        # `questdb.sh start` stays in the foreground as the JVM's parent, so
        # systemd can supervise it -- and it brings QuestDB's own JVM flags
        # (`AlwaysPreTouch`, the parallel collector, an error file inside the
        # data directory) that a hand-written `java -m ...ServerMain` would
        # quietly drop. `-n` disables its SIGHUP handler, which is systemd's
        # job; the default `KillMode` signals the whole control group, so the
        # JVM gets the SIGTERM directly rather than through the script.
        ExecStart = "${cfg.questdb.package}/bin/questdb.sh start -n -f -d ${cfg.questdb.dataDir}";
        Type = "simple";
        Restart = "always";
        RestartSec = 10;
        User = "questdb";
        Group = "questdb";
        # QuestDB checks these at start-up and refuses to run with the default
        # 1024: one file per column per partition adds up quickly.
        LimitNOFILE = 1048576;
        # systemd creates and chowns this, but only knows how to for a path
        # under /var/lib. A data directory elsewhere -- an external disk, say
        # -- has to exist and belong to the questdb user already.
        StateDirectory = lib.mkIf (lib.hasPrefix "/var/lib/" cfg.questdb.dataDir)
          (lib.removePrefix "/var/lib/" cfg.questdb.dataDir);
      };
    };

    users.users.questdb = lib.mkIf cfg.questdb.enable {
      isSystemUser = true;
      group = "questdb";
      home = cfg.questdb.dataDir;
      description = "QuestDB time-series database";
    };
    users.groups.questdb = lib.mkIf cfg.questdb.enable { };

    # No sysctl settings here, and that is not an oversight. QuestDB checks
    # `vm.max_map_count` and `fs.file-max` at start-up because its columns are
    # memory-mapped files, and NixOS already sets the first to exactly the
    # 1048576 it wants (`nixos/modules/config/sysctl.nix`, for unrelated
    # reasons) while the second is derived from RAM on any machine large enough
    # to run a database at all. Setting them again here is not harmless: a
    # second `mkDefault` at the same priority is a conflict, not a duplicate,
    # and the evaluation fails with "defined multiple times" -- which is how
    # this was found, on the first real build.
  };
}
