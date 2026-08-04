# SteelWorldGen

![GitHub License](https://img.shields.io/github/license/BlueDragonMC/SteelWorldGen)\*
![GitHub last commit](https://img.shields.io/github/last-commit/BlueDragonMC/SteelWorldGen)
![Minestom version](https://img.shields.io/badge/dynamic/toml?url=https%3A%2F%2Fraw.githubusercontent.com%2FBlueDragonMC%2FSteelWorldGen%2Fmain%2Fjava-client%2Fgradle%2Flibs.versions.toml&query=%24.versions.minestom&label=Minestom%20Version)

Uses [SteelMC](https://github.com/Steel-Foundation/SteelMC/) as a library to implement vanilla Minecraft world generation in a Minestom world generator.

<small>

_\* SteelMC is licensed under the AGPLv3 license. Only the Java libraries in this repo are Apache-2.0. See [LICENSE.md](./LICENSE.md) for more details._

</small>

## How it works

`steel-provider/src/lib.rs` contains some functions that interact with SteelMC to bring chunks through the full generation process outside of a normal server environment. Those functions are compiled into a standalone executable (`steel-provider/src/main.rs`), which acts as a "dumb" server that exclusively handles chunk generation.

The Java side is split into two modules in `java-client`: the `bridge` module is a standalone client library that connects to the steel-provider server, and the `minestom` module adapts it into a Minestom world generator. Each `Generator#generate()` call sends a small packet with the seed and chunk coordinates and then reads a response containing the generated chunk's sections in Minecraft's own network format.

The server can be used standalone. Currently, only a Java client exists, but other clients could easily be made as long as they understand how to decode the data structures in Minecraft's chunk data packet. For more details on the protocol, see [steel-provider/PROTOCOL.md](steel-provider/PROTOCOL.md).

## Installation

![Latest version](https://img.shields.io/badge/dynamic/xml?url=https%3A%2F%2Freposilite.bluedragonmc.com%2Freleases%2Fcom%2Fbluedragonmc%2Fsteelworldgen-minestom%2Fmaven-metadata.xml&query=%2Fmetadata%2Fversioning%2Flatest&label=Latest%20Version)

```kotlin
repositories {
   maven(url = "https://reposilite.bluedragonmc.com/releases")
}

dependencies {
   implementation("com.bluedragonmc:steelworldgen-minestom:$VERSION")
}
```

If you only need to talk to a steel-provider server without Minestom, depend on `com.bluedragonmc:steelworldgen-bridge` instead.

## Usage

```java
Instance instance = MinecraftServer.getInstanceManager().createInstanceContainer();

long seed = 42L;
instance.setGenerator(SteelWorldGenProvider.getGenerator(seed));
instance.setChunkSupplier(LightingChunk::new);
```

For a full example, see the `java-client/demo` directory. You can run the demo locally with `mise run demo`.
You'll probably want to use the `--release` flag (`mise run demo --release`).
Chunk generation gets MUCH faster at the expense of a longer compilation time.

## Building from Source

1. Install [`mise`](https://mise.jdx.dev/)

   We use `mise` to manage tools (like Java and Gradle) and to define tasks like you would in a Makefile.
   It's configured in `mise.toml`.

2. Run `mise run build`

   For a release (optimized) build, use `mise run build --release`.

   By default the Rust binary is built natively with `cargo build`. To instead cross-compile a fully static binary using [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild), pass `--static` (`mise run build --release --static`).

   The Java library will be built to `java-client/minestom/build/libs/minestom-dev.jar`. If you want to publish it to a Maven repository, modify the hostname in [java-client/minestom/build.gradle.kts](java-client/minestom/build.gradle.kts) and run `mise run publish` (or `mise run publishToMavenLocal` to run `gradle publishToMavenLocal`).

### AI Disclosure

The Rust portion of this project was written with a lot of AI assistance.
