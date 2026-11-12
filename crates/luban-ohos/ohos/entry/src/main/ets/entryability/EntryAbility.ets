import { AbilityConstant, Want, abilityAccessCtrl, Permissions } from "@kit.AbilityKit";
import { BusinessError } from "@kit.BasicServicesKit";
import { LazyPlugin, NativeAbility } from "@ohos-rs/ability";
import { AppControlPlugin } from "@ohos-rs/ability-plugin-app-control";
import { ClipboardPlugin } from "@ohos-rs/ability-plugin-clipboard";
import { FilesPlugin } from "@ohos-rs/ability-plugin-files";
import { MenuPlugin } from "@ohos-rs/ability-plugin-menu";
import { NotificationPlugin } from "@ohos-rs/ability-plugin-notification";
import { PermissionPlugin } from "@ohos-rs/ability-plugin-permission";
import { SystemStatePlugin } from "@ohos-rs/ability-plugin-system-state";
import { UrlPlugin } from "@ohos-rs/ability-plugin-url";
import { WindowPlugin } from "@ohos-rs/ability-plugin-window";
import hilog from "@ohos.hilog";
import * as nativeModule from "libluban_ohos.so";
import { FileIoPlugin } from "../common/FileIoPlugin";

const READ_PASTEBOARD: Permissions = "ohos.permission.READ_PASTEBOARD";
const LOG_DOMAIN: number = 0x0000;
const LOG_TAG: string = "LubanNotepad";

/**
 * Hosts the gpui-ohos platform. `moduleName` must match the Rust `[lib] name`
 * in `crates/luban-ohos/Cargo.toml`; the XComponent in `pages/Index.ets` loads
 * the same native module.
 */
export default class EntryAbility extends NativeAbility {
  public moduleName: string = "luban_ohos";
  public defaultPage: boolean = true;
  public bridgePlugins = [
    new LazyPlugin(() => new AppControlPlugin()),
    new LazyPlugin(() => new ClipboardPlugin()),
    new LazyPlugin(() => new FilesPlugin()),
    new LazyPlugin(() => new FileIoPlugin()),
    new LazyPlugin(() => new MenuPlugin()),
    new LazyPlugin(() => new NotificationPlugin({ scheme: "luban-notification" })),
    new LazyPlugin(() => new PermissionPlugin()),
    new LazyPlugin(() => new SystemStatePlugin()),
    new LazyPlugin(() => new UrlPlugin()),
    new LazyPlugin(() => new WindowPlugin()),
  ];

  private clipboardPermissionRequested: boolean = false;

  override onCreate(want: Want, launchParam: AbilityConstant.LaunchParam): void {
    if (typeof nativeModule.init !== "function") {
      throw new Error("Luban Notepad native module is unavailable");
    }
    super.onCreate(want, launchParam);
  }

  override onForeground(): void {
    super.onForeground();
    void this.requestClipboardPermission();
  }

  /**
   * READ_PASTEBOARD is user_grant: the signing profile's ACL only makes the
   * permission requestable. Ask once per process, after the ability is visible.
   */
  private async requestClipboardPermission(): Promise<void> {
    if (this.clipboardPermissionRequested) {
      return;
    }
    this.clipboardPermissionRequested = true;
    try {
      const manager = abilityAccessCtrl.createAtManager();
      const result = await manager.requestPermissionsFromUser(this.context, [READ_PASTEBOARD]);
      const grantResult = result.authResults[0] ?? -1;
      if (grantResult !== abilityAccessCtrl.GrantStatus.PERMISSION_GRANTED) {
        hilog.warn(LOG_DOMAIN, LOG_TAG, "READ_PASTEBOARD not granted: %{public}d", grantResult);
      }
    } catch (error) {
      // Let a later foreground retry if the first request ran before the
      // ability could host the system permission dialog.
      this.clipboardPermissionRequested = false;
      const err = error as BusinessError;
      hilog.error(
        LOG_DOMAIN,
        LOG_TAG,
        "READ_PASTEBOARD request failed: %{public}d %{public}s",
        err.code,
        err.message,
      );
    }
  }
}
