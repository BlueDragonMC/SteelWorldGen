plugins {
    id("org.gradle.toolchains.foojay-resolver-convention") version "1.0.0"
}

rootProject.name = "steel-worldgen-client"
include("bridge")
include("minestom")
include("demo")
