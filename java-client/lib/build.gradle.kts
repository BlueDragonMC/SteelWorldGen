import java.text.SimpleDateFormat
import java.util.*

plugins {
    java
    `java-library`
    `maven-publish`
}

group = "com.bluedragonmc"
version = getPublishingVersion()

repositories {
    mavenLocal()
    mavenCentral()
    maven(url = "https://reposilite.bluedragonmc.com/releases")
}

dependencies {
    compileOnly(libs.minestom)
    testImplementation(libs.minestom)
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

val sourcesJar by tasks.registering(Jar::class) {
    archiveClassifier.set("sources")
    from(sourceSets.main.get().allSource)
}

val javadocJar by tasks.registering(Jar::class) {
    archiveClassifier.set("javadoc")
    from(tasks.javadoc)
    archiveClassifier.set("javadoc")
}

fun getOutputOf(command: String): String? {
    try {
        val output = providers.exec {
            commandLine = command.split(" ")
        }
        return output.standardOutput.asText.get().trim()
    } catch (_: Throwable) {
        return null
    }
}

fun isInCI() = System.getenv("CI") != null

fun getPublishingVersion(): String = if (isInCI()) {
    val commitSha = getOutputOf("git rev-parse --verify --short HEAD")
    val date = SimpleDateFormat("YYYY-MM-dd").format(Date())

    "$date-$commitSha"
} else {
    "dev"
}

publishing {
    val rootVersion = project.version.toString()
    val inCI = rootVersion != "dev"
    repositories {
        if (inCI) {
            maven {
                name = "reposilite"
                url = uri("https://reposilite.bluedragonmc.com/releases")
                credentials(PasswordCredentials::class)
                authentication {
                    create<BasicAuthentication>("basic")
                }
            }
        }
    }
    publications {
        create<MavenPublication>("maven") {
            groupId = "com.bluedragonmc"
            artifactId = "steelworldgen"
            version = rootVersion

            from(components["java"])
            artifact(sourcesJar)
            artifact(javadocJar)
        }
    }
}
