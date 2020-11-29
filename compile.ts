const specifier = Deno.args[0];
const out = Deno.args[1];
const [_, bundle] = await Deno.bundle(specifier);

const originalBinaryPath = Deno.execPath();
const originalBin = await Deno.readFile(originalBinaryPath);

const encoder = new TextEncoder();
const bundleData = encoder.encode(bundle);
const magicTrailer = new Uint8Array(12);
magicTrailer.set(encoder.encode("DENO"), 0);
new DataView(magicTrailer.buffer).setBigUint64(
  4,
  BigInt(originalBin.byteLength),
  false,
);

var newBin = new Uint8Array(
  originalBin.byteLength + bundleData.byteLength + magicTrailer.byteLength,
);
newBin.set(originalBin, 0);
newBin.set(bundleData, originalBin.byteLength);
newBin.set(magicTrailer, originalBin.byteLength + bundleData.byteLength);

await Deno.writeFile(out, newBin);
console.log(`Created binary at ${out}`);
