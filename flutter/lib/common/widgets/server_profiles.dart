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
  /// Reads the saved list; entries that no longer decode are dropped.
  static Future<List<ServerProfile>> load() async {
    final raw = await bind.mainGetOption(key: kServerProfilesOption);
    return decode(raw);
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
