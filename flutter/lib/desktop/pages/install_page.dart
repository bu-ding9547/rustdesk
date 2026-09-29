import 'dart:async';
import 'dart:convert';

import 'package:file_picker/file_picker.dart';
import 'package:flutter/material.dart';
import 'package:flutter_hbb/common.dart';
import 'package:flutter_hbb/desktop/widgets/tabbar_widget.dart';
import 'package:flutter_hbb/models/platform_model.dart';
import 'package:flutter_hbb/models/state_model.dart';
import 'package:get/get.dart';
import 'package:path/path.dart';
import 'package:url_launcher/url_launcher_string.dart';
import 'package:window_manager/window_manager.dart';

/// The installer window. When the build was started from the update card there is a staged
/// upgrade (`update_pending_info()`): its first page shows what is about to happen - the version
/// step and the backup the client already made - and the second page is the plain install page.
/// After "Accept and Install" the same window keeps showing what the install does next: the
/// per-file hash check against the release's list, the repair of anything the installer could
/// not write, the cleanup and the registry sync. All of that runs in Rust, with no script.
class InstallPage extends StatefulWidget {
  const InstallPage({Key? key}) : super(key: key);

  @override
  State<InstallPage> createState() => _InstallPageState();
}

class _InstallPageState extends State<InstallPage> {
  final tabController = DesktopTabController(tabType: DesktopTabType.main);

  _InstallPageState() {
    Get.put<DesktopTabController>(tabController);
    const label = "install";
    tabController.add(TabInfo(
        key: label,
        label: label,
        closable: false,
        page: _InstallPageBody(
          key: const ValueKey(label),
        )));
  }

  @override
  void dispose() {
    super.dispose();
    Get.delete<DesktopTabController>();
  }

  @override
  Widget build(BuildContext context) {
    return DragToResizeArea(
      resizeEdgeSize: stateGlobal.resizeEdgeSize.value,
      enableResizeEdges: windowManagerEnableResizeEdges,
      child: Container(
        child: Scaffold(
            backgroundColor: Theme.of(context).colorScheme.background,
            body: DesktopTab(controller: tabController)),
      ),
    );
  }
}

/// What the client staged before it started this installer.
class _Pending {
  final String fromVersion;
  final String toVersion;
  final String installDir;
  final String backup;

  _Pending(this.fromVersion, this.toVersion, this.installDir, this.backup);

  static _Pending? parse(String json) {
    if (json.isEmpty) return null;
    try {
      final map = jsonDecode(json);
      if (map is! Map) return null;
      return _Pending(
        '${map['from_version'] ?? ''}',
        '${map['to_version'] ?? ''}',
        '${map['install_dir'] ?? ''}',
        '${map['backup'] ?? ''}',
      );
    } catch (_) {
      return null;
    }
  }
}

/// What the install is doing right now, straight from Rust.
class _Progress {
  final String step;
  final String text;
  final int done;
  final int total;
  final bool finished;
  final String error;

  _Progress(this.step, this.text, this.done, this.total, this.finished,
      this.error);

  static _Progress? parse(String json) {
    if (json.isEmpty) return null;
    try {
      final map = jsonDecode(json);
      if (map is! Map) return null;
      return _Progress(
        '${map['step'] ?? ''}',
        '${map['text'] ?? ''}',
        (map['done'] as num?)?.toInt() ?? 0,
        (map['total'] as num?)?.toInt() ?? 0,
        map['finished'] == true,
        '${map['error'] ?? ''}',
      );
    } catch (_) {
      return null;
    }
  }
}

class _InstallPageBody extends StatefulWidget {
  const _InstallPageBody({Key? key}) : super(key: key);

  @override
  State<_InstallPageBody> createState() => _InstallPageBodyState();
}

class _InstallPageBodyState extends State<_InstallPageBody>
    with WindowListener {
  late final TextEditingController controller;
  final RxBool startmenu = true.obs;
  final RxBool desktopicon = true.obs;
  final RxBool printer = false.obs;
  final RxBool showProgress = false.obs;
  final RxBool btnEnabled = true.obs;
  /// 0 = what is about to happen, 1 = the install page. The staged upgrade decides where it
  /// starts; a plain install has nothing to prepare and starts at 1.
  final RxInt step = 0.obs;
  final Rx<_Progress?> progress = Rx<_Progress?>(null);
  _Pending? pending;
  Timer? _poll;

  // todo move to theme.
  final buttonStyle = OutlinedButton.styleFrom(
    textStyle: TextStyle(fontSize: 14, fontWeight: FontWeight.normal),
    padding: EdgeInsets.symmetric(vertical: 15, horizontal: 12),
  );

  _InstallPageBodyState() {
    pending = _Pending.parse(bind.updatePendingInfo());
    // The staged directory wins over the registry: an earlier install may have cleared
    // InstallLocation, and then the default would be a fresh copy under Program Files instead
    // of an upgrade in place.
    final staged = pending?.installDir ?? '';
    controller = TextEditingController(
        text: staged.isNotEmpty ? staged : bind.installInstallPath());
    final installOptions = jsonDecode(bind.installInstallOptions());
    startmenu.value = installOptions['STARTMENUSHORTCUTS'] != '0';
    desktopicon.value = installOptions['DESKTOPSHORTCUTS'] != '0';
    printer.value = installOptions['PRINTER'] == '1';
    if (pending == null) {
      step.value = 1;
    }
  }

  @override
  void initState() {
    windowManager.addListener(this);
    super.initState();
  }

  @override
  void dispose() {
    _poll?.cancel();
    windowManager.removeListener(this);
    super.dispose();
  }

  @override
  void onWindowClose() {
    gFFI.close();
    super.onWindowClose();
    windowManager.setPreventClose(false);
    windowManager.close();
  }

  /// The install runs in Rust and keeps working after this window stops waiting; polling shows
  /// where it is without depending on events.
  void startPolling() {
    _poll?.cancel();
    _poll = Timer.periodic(const Duration(milliseconds: 500), (_) {
      final current = _Progress.parse(bind.updateProgressJson());
      if (current != null) {
        progress.value = current;
      }
    });
  }

  InkWell Option(RxBool option, {String label = ''}) {
    return InkWell(
      // todo mouseCursor: "SystemMouseCursors.forbidden" or no cursor on btnEnabled == false
      borderRadius: BorderRadius.circular(6),
      onTap: () => btnEnabled.value ? option.value = !option.value : null,
      child: Row(
        children: [
          Obx(
            () => Checkbox(
              visualDensity: VisualDensity(horizontal: -4, vertical: -4),
              value: option.value,
              onChanged: (v) =>
                  btnEnabled.value ? option.value = !option.value : null,
            ).marginOnly(right: 8),
          ),
          Expanded(
            child: Text(translate(label)),
          ),
        ],
      ),
    );
  }

  Widget _preparePage(BuildContext context, double em) {
    final staged = pending!;
    final isDarkTheme = MyTheme.currentThemeMode() == ThemeMode.dark;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Text(translate('Upgrade'),
            style: Theme.of(context).textTheme.headlineMedium),
        Row(
          children: [
            Text('${staged.fromVersion}  →  ${staged.toVersion}',
                style: Theme.of(context).textTheme.titleLarge),
          ],
        ).marginSymmetric(vertical: 2 * em),
        Text('${translate('Installation Path')}: ${staged.installDir}')
            .marginOnly(bottom: 8),
        if (staged.backup.isNotEmpty)
          Text('${translate('Backup')}: ${staged.backup}')
              .marginOnly(bottom: 8),
        Container(
            padding: EdgeInsets.all(12),
            decoration: BoxDecoration(
              color: isDarkTheme
                  ? Color.fromARGB(135, 87, 87, 90)
                  : Colors.grey[100],
              borderRadius: BorderRadius.circular(8),
              border: Border.all(color: Colors.grey),
            ),
            child: Row(children: [
              Icon(Icons.info_outline_rounded, size: 32).marginOnly(right: 16),
              Expanded(
                child: Text(translate(
                    'Downloaded and backed up, ready to install in place')),
              ),
            ])).marginSymmetric(vertical: 2 * em),
        Row(
          children: [
            Expanded(child: Container()),
            OutlinedButton.icon(
              icon: Icon(Icons.close_rounded, size: 16),
              label: Text(translate('Cancel')),
              onPressed: () => windowManager.close(),
              style: buttonStyle,
            ).marginOnly(right: 10),
            ElevatedButton.icon(
              icon: Icon(Icons.arrow_forward_rounded, size: 16),
              label: Text(translate('Next')),
              onPressed: () => step.value = 1,
              style: buttonStyle,
            ),
          ],
        ),
      ],
    );
  }

  Widget _progressArea(BuildContext context) {
    final double em = 13;
    return Obx(() {
      final current = progress.value;
      if (current == null) {
        return const Offstage();
      }
      final value = current.total > 0 ? current.done / current.total : null;
      return Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(current.text).marginOnly(bottom: 8),
          LinearProgressIndicator(value: value).marginOnly(bottom: 8),
          if (current.total > 0)
            Text('${current.done} / ${current.total}')
                .marginOnly(bottom: 8),
          if (current.error.isNotEmpty)
            Text(current.error, style: TextStyle(color: Colors.red)),
        ],
      ).marginOnly(top: em);
    });
  }

  @override
  Widget build(BuildContext context) {
    final double em = 13;
    final isDarkTheme = MyTheme.currentThemeMode() == ThemeMode.dark;
    return Scaffold(
        backgroundColor: null,
        body: SingleChildScrollView(
          child: Obx(() => step.value == 0 && pending != null
              ? _preparePage(context, em)
                  .paddingSymmetric(horizontal: 4 * em, vertical: 3 * em)
              : Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text(translate('Installation'),
                        style: Theme.of(context).textTheme.headlineMedium),
                    Row(
                      children: [
                        Text('${translate('Installation Path')}:')
                            .marginOnly(right: 10),
                        Expanded(
                          child: TextField(
                            controller: controller,
                            readOnly: true,
                            decoration: InputDecoration(
                              contentPadding: EdgeInsets.all(0.75 * em),
                            ),
                          ).workaroundFreezeLinuxMint().marginOnly(right: 10),
                        ),
                        Obx(
                          () => OutlinedButton.icon(
                            icon: Icon(Icons.folder_outlined, size: 16),
                            onPressed:
                                btnEnabled.value ? selectInstallPath : null,
                            style: buttonStyle,
                            label: Text(translate('Change Path')),
                          ),
                        )
                      ],
                    ).marginSymmetric(vertical: 2 * em),
                    Option(startmenu, label: 'Create start menu shortcuts')
                        .marginOnly(bottom: 7),
                    Option(desktopicon, label: 'Create desktop icon')
                        .marginOnly(bottom: 7),
                    Option(printer, label: 'Install {$appName} Printer'),
                    Container(
                        padding: EdgeInsets.all(12),
                        decoration: BoxDecoration(
                          color: isDarkTheme
                              ? Color.fromARGB(135, 87, 87, 90)
                              : Colors.grey[100],
                          borderRadius: BorderRadius.circular(8),
                          border: Border.all(color: Colors.grey),
                        ),
                        child: Row(
                          children: [
                            Icon(Icons.info_outline_rounded, size: 32)
                                .marginOnly(right: 16),
                            Column(
                              crossAxisAlignment: CrossAxisAlignment.start,
                              children: [
                                Text(translate('agreement_tip'))
                                    .marginOnly(bottom: em),
                                InkWell(
                                  hoverColor: Colors.transparent,
                                  onTap: () => launchUrlString(
                                      'https://rustdesk.com/privacy.html'),
                                  child: Tooltip(
                                    message:
                                        'https://rustdesk.com/privacy.html',
                                    child: Row(children: [
                                      Icon(Icons.launch_outlined, size: 16)
                                          .marginOnly(right: 5),
                                      Text(
                                        translate(
                                            'End-user license agreement'),
                                        style: const TextStyle(
                                            decoration:
                                                TextDecoration.underline),
                                      )
                                    ]),
                                  ),
                                ),
                              ],
                            )
                          ],
                        )).marginSymmetric(vertical: 2 * em),
                    _progressArea(context),
                    Row(
                      children: [
                        Expanded(
                          // NOT use Offstage to wrap LinearProgressIndicator
                          child: Obx(() => showProgress.value
                              ? LinearProgressIndicator().marginOnly(right: 10)
                              : Offstage()),
                        ),
                        Obx(
                          () => OutlinedButton.icon(
                            icon: Icon(Icons.close_rounded, size: 16),
                            label: Text(translate('Cancel')),
                            onPressed: btnEnabled.value
                                ? () => windowManager.close()
                                : null,
                            style: buttonStyle,
                          ).marginOnly(right: 10),
                        ),
                        Obx(
                          () => ElevatedButton.icon(
                            icon: Icon(Icons.done_rounded, size: 16),
                            label: Text(translate('Accept and Install')),
                            onPressed: btnEnabled.value ? install : null,
                            style: buttonStyle,
                          ),
                        ),
                        Offstage(
                          offstage: bind.installShowRunWithoutInstall(),
                          child: Obx(
                            () => OutlinedButton.icon(
                              icon: Icon(Icons.screen_share_outlined, size: 16),
                              label: Text(translate('Run without install')),
                              onPressed: btnEnabled.value
                                  ? () => bind.installRunWithoutInstall()
                                  : null,
                              style: buttonStyle,
                            ).marginOnly(left: 10),
                          ),
                        ),
                      ],
                    )
                  ],
                ).paddingSymmetric(horizontal: 4 * em, vertical: 3 * em)),
        ));
  }

  void install() {
    do_install() {
      btnEnabled.value = false;
      showProgress.value = true;
      // The window stays up while Rust installs and then checks every file.
      startPolling();
      String args = '';
      if (startmenu.value) args += ' startmenu';
      if (desktopicon.value) args += ' desktopicon';
      if (printer.value) args += ' printer';
      bind.installInstallMe(options: args, path: controller.text);
    }

    do_install();
  }

  void selectInstallPath() async {
    String? install_path = await FilePicker.platform
        .getDirectoryPath(initialDirectory: controller.text);
    if (install_path != null) {
      controller.text = join(install_path, await bind.mainGetAppName());
    }
  }
}
