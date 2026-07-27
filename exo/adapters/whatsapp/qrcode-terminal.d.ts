declare module "qrcode-terminal" {
  const qrcodeTerminal: {
    generate(
      input: string,
      options: { small?: boolean },
      callback: (qr: string) => void,
    ): void;
  };
  export default qrcodeTerminal;
  export function generate(
    input: string,
    options: { small?: boolean },
    callback: (qr: string) => void,
  ): void;
}
