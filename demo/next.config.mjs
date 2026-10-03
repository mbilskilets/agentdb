import path from "node:path";

const repository = path.join(import.meta.dirname, "..");

/** @type {import('next').NextConfig} */
const config = {
  // The SDK is linked from ../sdk, so the build has to see the whole repository.
  turbopack: { root: repository },
  outputFileTracingRoot: repository,
  serverExternalPackages: ["agentdb"],
  allowedDevOrigins: ["*.exe.xyz"],
};

export default config;
