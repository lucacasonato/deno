const filereader = new FileReader();
filereader.onabort = () => {
  console.log("abort");
  queueMicrotask(() => console.log("mt abort"));
};
filereader.onerror = () => {
  console.log("error");
  queueMicrotask(() => console.log("mt error"));
};
filereader.onload = () => {
  console.log("load");
  queueMicrotask(() => console.log("mt load"));
};
filereader.onloadend = () => {
  console.log("loadend");
  queueMicrotask(() => console.log("mt loadend"));
};
filereader.onloadstart = () => {
  console.log("loadstart");
  queueMicrotask(() => console.log("mt loadstart"));
};
filereader.onprogress = () => {
  console.log("progress");
  queueMicrotask(() => console.log("mt progress"));
};
filereader.readAsText(new Blob([]));

// queueMicrotask(() => console.log("microtask"));
// Deno.core.performMicrotaskCheckpoint();
// console.log("post performMicrotaskCheckpoint");
