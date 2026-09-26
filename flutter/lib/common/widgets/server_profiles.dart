import 'dart:convert';

import 'package:flutter/material.dart';

import '../../common.dart';
import '../../models/platform_model.dart';

/// Shared option holding the user's server list. The Rust side reads the same
/// key and runs every enabled entry at the same time.
const kServerProfilesOption = 'server-profiles';

/// One saved server: a title plus the four fields of the original dialog.
class ServerProfile {
  String title;
  ServerConfig config;
  bool enabled;

  /// The entry whose API server (login, address book, devices) is used.
  bool primary;

  ServerProfile({
    required this.title,
    required this.config,
    this.enabled = true,
    this.primary = false,
  });

  Map<String, dynamic> toJson() => {
        'title': title,
        'id': config.idServer,
        'relay': config.relayServer,
        'api': config.apiServer,
        'key': config.key,
        'enabled': enabled,
        'primary': primary,
      };

  static ServerProfile? fromJson(dynamic value) {
    if (value is! Map) return null;
    final id = value['id']?.toString() ?? '';
    final title = value['title']?.toString() ?? '';
    if (id.trim().isEmpty && title.trim().isEmpty) return null;
    return ServerProfile(
      title: title.trim().isEmpty ? id.trim() : title.trim(),
      config: ServerConfig(
        idServer: id,
        relayServer: value['relay']?.toString(),
        apiServer: value['api']?.toString(),
        key: value['key']?.toString(),
      ),
      enabled: value['enabled'] is bool ? value['enabled'] as bool : true,
      primary: value['primary'] == true,
    );
  }
}

class ServerProfiles {
  /// Marker inside the export payload, so an import can tell a whole-list string
  /// from the legacy single-server one.
  static const exportMarker = 'rustdesk-servers';
  static const exportVersion = 1;

  /// Reads the saved list; entries that no longer decode are dropped.
  static Future<List<ServerProfile>> load() async {
    final raw = await bind.mainGetOption(key: kServerProfilesOption);
    return decode(raw);
  }

  /// Encodes every server into one shareable string: pasting it into another
  /// machine restores the whole list, keys included.
  static String encodeAll(List<ServerProfile> profiles) {
    final payload = jsonEncode({
      'type': exportMarker,
      'version': exportVersion,
      'servers': [for (final p in profiles) p.toJson()],
    });
    return base64UrlEncode(utf8.encode(payload)).split('').reversed.join();
  }

  /// Decodes a whole-list string; null when [text] is not one (a legacy
  /// single-server string, or anything unreadable).
  static List<ServerProfile>? decodeAll(String? text) {
    final trimmed = text?.trim() ?? '';
    if (trimmed.isEmpty) {
      return null;
    }
    // Accept both our reversed form and a plain base64 paste.
    for (final candidate in {trimmed, trimmed.split('').reversed.join()}) {
      try {
        final decoded = utf8.decode(base64Decode(base64.normalize(candidate)));
        final json = jsonDecode(decoded);
        if (json is! Map || json['type'] != exportMarker) {
          continue;
        }
        final servers = json['servers'];
        if (servers is! List) {
          continue;
        }
        final profiles = servers
            .map(ServerProfile.fromJson)
            .whereType<ServerProfile>()
            .toList();
        if (profiles.isNotEmpty) {
          return profiles;
        }
      } catch (_) {
        // not this format; try the next candidate
      }
    }
    return null;
  }

  static List<ServerProfile> decode(String raw) {
    if (raw.trim().isEmpty) {
      return [];
    }
    try {
      final decoded = jsonDecode(raw);
      if (decoded is! List) {
        return [];
      }
      return decoded
          .map(ServerProfile.fromJson)
          .whereType<ServerProfile>()
          .toList();
    } catch (e) {
      debugPrint('Invalid server profiles: $e');
      return [];
    }
  }

  static Future<void> save(List<ServerProfile> profiles) => bind.mainSetOption(
      key: kServerProfilesOption,
      value: jsonEncode(profiles.map((e) => e.toJson()).toList()));
}

/// Paste / copy buttons for the whole list, matching the pair the server dialog
/// used when it only handled one server.
List<Widget> serverProfilesImportExportWidgets({
  required VoidCallback onImport,
  required VoidCallback onExport,
}) =>
    [
      Tooltip(
        message: translate('Import server config'),
        child:
            IconButton(icon: Icon(Icons.paste, color: Colors.grey), onPressed: onImport),
      ),
      Tooltip(
          message: translate('Export Server Config'),
          child: IconButton(
              icon: Icon(Icons.copy, color: Colors.grey), onPressed: onExport)),
    ];
