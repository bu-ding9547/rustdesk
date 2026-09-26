import 'dart:async';
import 'dart:convert';
import 'package:flutter/material.dart';
import 'package:flutter_hbb/common/widgets/server_profiles.dart';
import 'package:flutter_hbb/common/widgets/setting_widgets.dart';
import 'package:flutter_hbb/common/widgets/toolbar.dart';
import 'package:get/get.dart';

import '../../common.dart';
import '../../models/platform_model.dart';

void _showSuccess() {
  showToast(translate("Successful"));
}

void setTemporaryPasswordLengthDialog(
    OverlayDialogManager dialogManager) async {
  List<String> lengths = ['6', '8', '10'];
  String length = await bind.mainGetOption(key: "temporary-password-length");
  var index = lengths.indexOf(length);
  if (index < 0) index = 0;
  length = lengths[index];
  dialogManager.show((setState, close, context) {
    setLength(newValue) {
      final oldValue = length;
      if (oldValue == newValue) return;
      setState(() {
        length = newValue;
      });
      bind.mainSetOption(key: "temporary-password-length", value: newValue);
      bind.mainUpdateTemporaryPassword();
      Future.delayed(Duration(milliseconds: 200), () {
        close();
        _showSuccess();
      });
    }

    return CustomAlertDialog(
      title: Text(translate("Set one-time password length")),
      content: Row(
          mainAxisAlignment: MainAxisAlignment.spaceEvenly,
          children: lengths
              .map(
                (value) => Row(
                  children: [
                    Text(value),
                    Radio(
                        value: value, groupValue: length, onChanged: setLength),
                  ],
                ),
              )
              .toList()),
    );
  }, backDismiss: true, clickMaskDismiss: true);
}

void showServerSettings(OverlayDialogManager dialogManager,
    void Function(VoidCallback) setState) async {
  Map<String, dynamic> options = {};
  try {
    options = jsonDecode(await bind.mainGetOptions());
  } catch (e) {
    print("Invalid server config: $e");
  }
  showServerSettingsWithValue(
      ServerConfig.fromOptions(options), dialogManager, setState);
}

void showServerSettingsWithValue(
    ServerConfig serverConfig,
    OverlayDialogManager dialogManager,
    void Function(VoidCallback)? upSetState) async {
  var isInProgress = false;
  final profiles = await ServerProfiles.load();
  if (profiles.isEmpty) {
    // Seed from what is configured today so the list is never empty.
    profiles.add(ServerProfile(
      title: serverConfig.idServer.isEmpty
          ? translate('Server')
          : serverConfig.idServer,
      config: serverConfig,
      primary: true,
    ));
  } else if (!profiles.any((p) => p.primary && p.enabled)) {
    profiles.first.primary = true;
  }
  final titleCtrls = [
    for (final p in profiles) TextEditingController(text: p.title)
  ];
  final idCtrls = [
    for (final p in profiles) TextEditingController(text: p.config.idServer)
  ];
  final relayCtrls = [
    for (final p in profiles) TextEditingController(text: p.config.relayServer)
  ];
  final apiCtrls = [
    for (final p in profiles) TextEditingController(text: p.config.apiServer)
  ];
  final keyCtrls = [
    for (final p in profiles) TextEditingController(text: p.config.key)
  ];
  final errMsgs = [for (final _ in profiles) ''.obs];
  var pendingDelete = -1;

  dialogManager.show((setState, close, context) {
    void insertProfile() {
      profiles.add(ServerProfile(title: '', config: ServerConfig()));
      titleCtrls.add(TextEditingController());
      idCtrls.add(TextEditingController());
      relayCtrls.add(TextEditingController());
      apiCtrls.add(TextEditingController());
      keyCtrls.add(TextEditingController());
      errMsgs.add(''.obs);
    }

    void removeProfile(int i) {
      profiles.removeAt(i);
      titleCtrls.removeAt(i).dispose();
      idCtrls.removeAt(i).dispose();
      relayCtrls.removeAt(i).dispose();
      apiCtrls.removeAt(i).dispose();
      keyCtrls.removeAt(i).dispose();
      errMsgs.removeAt(i);
      if (profiles.isNotEmpty && !profiles.any((p) => p.primary && p.enabled)) {
        profiles.first.primary = true;
      }
    }

    Future<bool> submit() async {
      setState(() {
        isInProgress = true;
      });
      // Drop entries the user left completely empty.
      for (var i = profiles.length - 1; i >= 0; i--) {
        if (idCtrls[i].text.trim().isEmpty && titleCtrls[i].text.trim().isEmpty) {
          removeProfile(i);
        }
      }
      for (var i = 0; i < profiles.length; i++) {
        errMsgs[i].value = '';
        profiles[i].title = titleCtrls[i].text.trim().isEmpty
            ? idCtrls[i].text.trim()
            : titleCtrls[i].text.trim();
        profiles[i].config = ServerConfig(
            idServer: idCtrls[i].text.trim(),
            relayServer: relayCtrls[i].text.trim(),
            apiServer: apiCtrls[i].text.trim(),
            key: keyCtrls[i].text.trim());
      }
      if (profiles.isEmpty) {
        setState(() {
          isInProgress = false;
        });
        return false;
      }
      if (!profiles.any((p) => p.primary && p.enabled)) {
        profiles
            .firstWhere((p) => p.enabled, orElse: () => profiles.first)
            .primary = true;
      }
      for (var i = 0; i < profiles.length; i++) {
        final profile = profiles[i];
        if (!profile.enabled) {
          continue;
        }
        if (profile.config.idServer.isEmpty) {
          errMsgs[i].value = translate('Invalid server configuration');
          setState(() {
            isInProgress = false;
          });
          return false;
        }
        final api = profile.config.apiServer;
        if (api.isNotEmpty &&
            !api.startsWith('http://') &&
            !api.startsWith('https://')) {
          errMsgs[i].value =
              '${translate("API Server")}: ${translate("invalid_http")}';
          setState(() {
            isInProgress = false;
          });
          return false;
        }
        final msg = translate(await bind.mainTestIfValidServer(
            server: profile.config.idServer, testWithProxy: true));
        if (msg.isNotEmpty) {
          errMsgs[i].value = msg;
          setState(() {
            isInProgress = false;
          });
          return false;
        }
      }
      // Keep the legacy single-server options in step with the primary entry:
      // the rest of the app still reads them, and this also logs out of an API
      // server we are leaving.
      final primary = profiles.firstWhere((p) => p.primary && p.enabled,
          orElse: () => profiles.firstWhere((p) => p.enabled,
              orElse: () => profiles.first));
      await setServerConfig(null, null, primary.config);
      await ServerProfiles.save(profiles);
      setState(() {
        isInProgress = false;
      });
      return true;
    }

    Widget buildField(
        String label, TextEditingController controller, String errorMsg,
        {String? Function(String?)? validator, bool autofocus = false}) {
      if (isDesktop || isWeb) {
        return Row(
          children: [
            SizedBox(
              width: 120,
              child: Text(label),
            ),
            SizedBox(width: 8),
            Expanded(
              child: serverSettingsTextFormField(
                label: label,
                controller: controller,
                errorMsg: errorMsg,
                contentPadding:
                    EdgeInsets.symmetric(horizontal: 8, vertical: 12),
                showLabelText: false,
                validator: validator,
                autofocus: autofocus,
              ).workaroundFreezeLinuxMint(),
            ),
          ],
        );
      }

      return serverSettingsTextFormField(
        label: label,
        controller: controller,
        errorMsg: errorMsg,
        validator: validator,
      ).workaroundFreezeLinuxMint();
    }

    Widget buildEntry(int i) {
      final profile = profiles[i];
      return Container(
        margin: const EdgeInsets.symmetric(vertical: 4),
        decoration: BoxDecoration(
          border: Border.all(color: Theme.of(context).dividerColor),
          borderRadius: BorderRadius.circular(8),
        ),
        child: ExpansionTile(
          key: ValueKey('server-profile-$i'),
          initiallyExpanded: profiles.length == 1,
          leading: Tooltip(
            message: translate('Default'),
            child: Radio<bool>(
              value: true,
              groupValue: profile.primary,
              onChanged: profile.enabled
                  ? (v) {
                      setState(() {
                        for (final other in profiles) {
                          other.primary = false;
                        }
                        profile.primary = true;
                      });
                    }
                  : null,
            ),
          ),
          title: ValueListenableBuilder<TextEditingValue>(
            valueListenable: titleCtrls[i],
            builder: (context, value, child) => Text(
              value.text.trim().isEmpty
                  ? translate('Server')
                  : value.text.trim(),
              overflow: TextOverflow.ellipsis,
              maxLines: 1,
            ),
          ),
          subtitle: ValueListenableBuilder<TextEditingValue>(
            valueListenable: idCtrls[i],
            builder: (context, value, child) => Text(
              value.text.trim(),
              overflow: TextOverflow.ellipsis,
              maxLines: 1,
            ),
          ),
          trailing: Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              Switch(
                value: profile.enabled,
                onChanged: (v) {
                  setState(() {
                    profile.enabled = v;
                    if (!v) {
                      profile.primary = false;
                      if (!profiles.any((p) => p.primary && p.enabled)) {
                        profiles
                            .firstWhere((p) => p.enabled,
                                orElse: () => profiles.first)
                            .primary = true;
                      }
                    }
                  });
                },
              ),
              if (pendingDelete == i)
                TextButton(
                  onPressed: () {
                    setState(() {
                      removeProfile(i);
                      pendingDelete = -1;
                    });
                  },
                  child: Text(translate('Confirm Delete'),
                      style: const TextStyle(color: Colors.red)),
                )
              else
                IconButton(
                  icon: const Icon(Icons.delete_outline, size: 18),
                  tooltip: translate('Delete'),
                  onPressed: () {
                    setState(() {
                      pendingDelete = i;
                    });
                  },
                ),
            ],
          ),
          childrenPadding: const EdgeInsets.fromLTRB(12, 0, 12, 12),
          children: [
            buildField(translate('Name'), titleCtrls[i], ''),
            const SizedBox(height: 8),
            buildField(translate('ID Server'), idCtrls[i], errMsgs[i].value,
                autofocus: i == 0),
            const SizedBox(height: 8),
            if (!isIOS && !isWeb) ...[
              buildField(translate('Relay Server'), relayCtrls[i], ''),
              const SizedBox(height: 8),
            ],
            buildField(
              translate('API Server'),
              apiCtrls[i],
              '',
              validator: (v) {
                if (v != null && v.isNotEmpty) {
                  if (!(v.startsWith('http://') || v.startsWith("https://"))) {
                    return translate("invalid_http");
                  }
                }
                return null;
              },
            ),
            const SizedBox(height: 8),
            buildField('Key', keyCtrls[i], ''),
          ],
        ),
      );
    }

    return CustomAlertDialog(
      title: Row(
        children: [
          Expanded(child: Text(translate('ID/Relay Server'))),
          if (idCtrls.isNotEmpty)
            ...ServerConfigImportExportWidgets(
              [idCtrls[0], relayCtrls[0], apiCtrls[0], keyCtrls[0]],
              [errMsgs[0], errMsgs[0], errMsgs[0]],
            ),
        ],
      ),
      content: ConstrainedBox(
        constraints: const BoxConstraints(minWidth: 500, maxHeight: 520),
        child: SizedBox(
          width: 520,
          child: Form(
            child: SingleChildScrollView(
              child: Obx(() => Column(
                    mainAxisSize: MainAxisSize.min,
                    children: [
                      for (var i = 0; i < profiles.length; i++) buildEntry(i),
                      Align(
                        alignment: Alignment.centerLeft,
                        child: TextButton.icon(
                          icon: const Icon(Icons.add, size: 18),
                          label: Text(translate('Add')),
                          onPressed: () {
                            setState(() {
                              insertProfile();
                              pendingDelete = -1;
                            });
                          },
                        ),
                      ),
                      if (isInProgress)
                        const Padding(
                          padding: EdgeInsets.only(top: 8),
                          child: LinearProgressIndicator(),
                        ),
                    ],
                  )),
            ),
          ),
        ),
      ),
      actions: [
        dialogButton('Cancel', onPressed: () {
          close();
        }, isOutline: true),
        dialogButton(
          'OK',
          onPressed: () async {
            if (await submit()) {
              close();
              showToast(translate('Successful'));
              upSetState?.call(() {});
            } else {
              showToast(translate('Failed'));
            }
          },
        ),
      ],
    );
  });
}

TextFormField serverSettingsTextFormField({
  required String label,
  required TextEditingController controller,
  required String errorMsg,
  String? Function(String?)? validator,
  bool autofocus = false,
  bool showLabelText = true,
  EdgeInsetsGeometry? contentPadding,
}) {
  return TextFormField(
    controller: controller,
    decoration: InputDecoration(
      labelText: showLabelText ? label : null,
      errorText: errorMsg.isEmpty ? null : errorMsg,
      contentPadding: contentPadding,
    ),
    validator: validator,
    autofocus: autofocus,
    keyboardType: TextInputType.visiblePassword,
    textCapitalization: TextCapitalization.none,
    autocorrect: false,
    enableSuggestions: false,
    smartDashesType: SmartDashesType.disabled,
    smartQuotesType: SmartQuotesType.disabled,
    enableIMEPersonalizedLearning: false,
    spellCheckConfiguration: const SpellCheckConfiguration.disabled(),
  );
}

void setPrivacyModeDialog(
  OverlayDialogManager dialogManager,
  List<TToggleMenu> privacyModeList,
  RxString privacyModeState,
) async {
  dialogManager.dismissAll();
  dialogManager.show((setState, close, context) {
    return CustomAlertDialog(
      title: Text(translate('Privacy mode')),
      content: Column(
          mainAxisAlignment: MainAxisAlignment.spaceEvenly,
          children: privacyModeList
              .map((value) => CheckboxListTile(
                    contentPadding: EdgeInsets.zero,
                    visualDensity: VisualDensity.compact,
                    title: value.child,
                    value: value.value,
                    onChanged: value.onChanged,
                  ))
              .toList()),
    );
  }, backDismiss: true, clickMaskDismiss: true);
}
