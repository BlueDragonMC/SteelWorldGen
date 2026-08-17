plugins {
    application
    id("com.gradleup.shadow") version "9.6.1"
}

repositories {
    mavenCentral()
}

dependencies {
    implementation(libs.minestom)
    implementation(project(":minestom"))
}

testing {
    suites {
        val test = named<JvmTestSuite>("test") {
            useJUnitJupiter("6.0.1")
        }
    }
}

java {
    toolchain {
        languageVersion = JavaLanguageVersion.of(25)
    }
}

tasks.build {
    dependsOn(tasks.shadowJar)
}

tasks.shadowJar {
    duplicatesStrategy = DuplicatesStrategy.INCLUDE
    mergeServiceFiles()
    // The demo jar's entry point is Bench, which dispatches on the environment:
    //   PREGEN_SIZE=N  -> square pregen benchmark (Steel harness markers)
    //   otherwise      -> the playable server
    mainClass.set("com.bluedragonmc.steelworldgen.demo.Bench")
}

application {
    mainClass.set("com.bluedragonmc.steelworldgen.demo.Main")
}
