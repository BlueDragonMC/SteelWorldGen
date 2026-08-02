# SteelWorldGen

![GitHub License](https://img.shields.io/github/license/BlueDragonMC/SteelWorldGen)
![GitHub last commit](https://img.shields.io/github/last-commit/BlueDragonMC/SteelWorldGen)
![Minestom version](https://img.shields.io/badge/dynamic/toml?url=https%3A%2F%2Fraw.githubusercontent.com%2FBlueDragonMC%2FSteelWorldGen%2Fmain%2Fjava-client%2Fgradle%2Flibs.versions.toml&query=%24.versions.minestom&label=Minestom%20Version)

Uses [SteelMC](https://github.com/Steel-Foundation/SteelMC/) as a library to implement vanilla Minecraft world generation in a Minestom world generator.

## How it works

`steel-provider/src/lib.rs` contains some functions that interact with SteelMC to bring chunks through the full generation process outside of a normal server environment.
Those functions are made available to Java via `steel-provider/src/c_api.rs`, which acts as a bridge that uses the C ABI.

A C header file is generated from `c_api.rs` using [`cbindgen`](https://github.com/mozilla/cbindgen).

Then, that header file is used to generate Java bindings with [`jextract`](https://github.com/openjdk/jextract).

Using those bindings, we can call the Rust functions from Java using the [Foreign Function and Memory API](https://docs.oracle.com/en/java/javase/21/core/foreign-function-and-memory-api.html).

`SteelWorldGenProvider` extracts a C shared library from inside the library JAR the first time a world generator is created.

## Installation

![Latest version](https://img.shields.io/badge/dynamic/xml?url=https%3A%2F%2Freposilite.bluedragonmc.com%2Freleases%2Fcom%2Fbluedragonmc%2Fsteelworldgen%2Fmaven-metadata.xml&query=%2Fmetadata%2Fversioning%2Flatest&label=Latest%20Version)

```kotlin
repositories {
   maven(url = "https://reposilite.bluedragonmc.com/releases")
}

dependencies {
   implementation("com.bluedragonmc:steelworldgen:$VERSION")
}
```

## Usage

```java
Instance instance = MinecraftServer.getInstanceManager().createInstanceContainer();

long seed = 42L;
instance.setGenerator(SteelWorldGenProvider.getGenerator(seed));
instance.setChunkSupplier(LightingChunk::new);
```

For a full example, see the `java-client/demo` directory. You can run the demo locally with `mise run demo`.

## Building from Source

1. Install [`mise`](https://mise.jdx.dev/)

   We use `mise` to manage tools (like Java and Gradle) and to define tasks like you would in a Makefile.
   It's configured in `mise.toml`.

2. Run `mise run build`

   For a release (optimized) build, use `mise run build --release`.

   Dependencies are already configured to build in release mode even without that flag, so you probably won't notice a significant performance difference. This is necessary because in debug mode, SteelMC generates a function that is so large that it overflows Java's default 1MB stack size. Compiling with optimizations makes the function small enough that we don't need to customize Java's stack size with the `-Xss` option.

   The Java library will be built to `java-client/lib/build/libs/lib.jar`.

### AI Disclosure

`steel-provider/src/lib.rs` (the part of the project that interacts directly with SteelMC) was written with a lot of AI assistance.
