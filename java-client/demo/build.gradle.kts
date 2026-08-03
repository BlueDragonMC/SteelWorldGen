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
}

application {
    mainClass.set("com.bluedragonmc.steelworldgen.demo.Main")
}
