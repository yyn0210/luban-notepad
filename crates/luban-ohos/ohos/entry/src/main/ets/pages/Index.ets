import { DefaultXComponent } from "@ohos-rs/ability";

@Entry
@Component
struct Index {
  build() {
    RelativeContainer() {
      DefaultXComponent({ moduleName: "luban_ohos" })
        .width("100%")
        .height("100%");
    }
    .width("100%")
    .height("100%");
  }
}
