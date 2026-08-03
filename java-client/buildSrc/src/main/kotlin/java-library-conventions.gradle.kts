import org.gradle.api.plugins.jvm.JvmTestSuite
import org.gradle.jvm.toolchain.JavaLanguageVersion

plugins {
    java
    `java-library`
}

group = "com.bluedragonmc"

repositories {
    mavenLocal()
    mavenCentral()
}

testing {
    suites {
        val test = named<JvmTestSuite>("test") {
            useJUnitJupiter("6.0.1")
        }
    }
}

tasks.test {
    testLogging.showStandardStreams = true
}

java {
    toolchain {
        languageVersion = JavaLanguageVersion.of(25)
    }
}
