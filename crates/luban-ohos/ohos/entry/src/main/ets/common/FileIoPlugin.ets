import fs from "@ohos.file.fs";
import {
  AsyncPluginBase,
  BridgeCallContext,
  BridgeContextRequirement,
  BridgeTypedValue,
} from "@ohos-rs/ability";

const WRITE_REQUEST_TYPE = "luban.file_io.WriteUriRequest";
const WRITE_RESPONSE_TYPE = "luban.file_io.WriteUriResponse";

interface WriteUriRequest {
  uri: string;
  bytes: number[];
}

class WriteUriResponse {
  readonly bytesWritten: number;

  constructor(bytesWritten: number) {
    this.bytesWritten = bytesWritten;
  }
}

/**
 * Writes document-picker URIs through @ohos.file.fs.
 *
 * The sandbox cannot open a user-selected path with POSIX calls, but the
 * picker URI is readable and writable through the file-management service.
 */
export class FileIoPlugin extends AsyncPluginBase {
  readonly id = "luban.file-io";
  readonly requires: BridgeContextRequirement[] = ["ability"];

  /**
   * Serializes writes: opener/truncate and write are separate asynchronous
   * calls, so without this an earlier save could truncate the file after a
   * later save had already written it.
   */
  private writeChain: Promise<void> = Promise.resolve();

  async invokeAsync(
    action: string,
    payload: BridgeTypedValue,
    context: BridgeCallContext,
  ): Promise<BridgeTypedValue> {
    if (action !== "write-uri") {
      throw new Error(`Unsupported luban.file-io action '${action}'`);
    }
    if (!context.isActive() || context.isCancelled()) {
      throw new Error("File I/O bridge session is not active");
    }
    if (
      payload.typeName !== WRITE_REQUEST_TYPE ||
      typeof payload.value !== "object" ||
      payload.value === null
    ) {
      throw new Error(`write-uri requires bridge type ${WRITE_REQUEST_TYPE}`);
    }
    const request = payload.value as WriteUriRequest;
    if (typeof request.uri !== "string" || request.uri.length === 0) {
      throw new Error("write-uri.uri must be a non-empty string");
    }
    if (!Array.isArray(request.bytes)) {
      throw new Error("write-uri.bytes must be an array");
    }

    const data = Uint8Array.from(request.bytes);
    const run = this.writeChain.then(() => this.writeFile(request.uri, data));
    // Keep the chain alive after a failed write; the caller still sees the
    // original rejection through `run`.
    this.writeChain = run.then(
      () => undefined,
      () => undefined,
    );
    const written = await run;
    return {
      typeName: WRITE_RESPONSE_TYPE,
      value: new WriteUriResponse(written),
    };
  }

  private async writeFile(uri: string, data: Uint8Array): Promise<number> {
    const file = await fs.open(
      uri,
      fs.OpenMode.READ_WRITE | fs.OpenMode.CREATE | fs.OpenMode.TRUNC,
    );
    let written = 0;
    try {
      if (data.byteLength > 0) {
        written = await fs.write(file.fd, data.buffer);
      }
      if (written !== data.byteLength) {
        throw new Error(`short write: ${written}/${data.byteLength} bytes`);
      }
    } finally {
      await fs.close(file);
    }
    return written;
  }
}
