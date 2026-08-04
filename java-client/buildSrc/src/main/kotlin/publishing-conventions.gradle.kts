import java.text.SimpleDateFormat
import java.util.Date

plugins {
    java
    `maven-publish`
}

// Shared publishing setup for publishable modules. Artifact coordinates are derived
// from the module name: com.bluedragonmc:steelworldgen-<module>. Modules versioned "dev"
// are not published to the remote repository.
group = "com.bluedragonmc"
version = getPublishingVersion()

val sourcesJar = tasks.register<Jar>("sourcesJar") {
    archiveClassifier.set("sources")
    from(sourceSets.main.get().allJava)
}

val javadocJar = tasks.register<Jar>("javadocJar") {
    archiveClassifier.set("javadoc")
    from(tasks.javadoc)
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
            artifactId = "steelworldgen-${project.name}"
            version = rootVersion

            from(components["java"])
            artifact(sourcesJar)
            artifact(javadocJar)

            pom {
                licenses {
                    license {
                        name = "Apache License, Version 2.0"
                        url = "https://www.apache.org/licenses/LICENSE-2.0.txt"
                        distribution = "repo"
                    }
                    if (project.name == "bridge") {
                        license {
                            name = "GNU Affero General Public License, Version 3"
                            url = "https://www.gnu.org/licenses/agpl-3.0.txt"
                            distribution = "repo"
                        }
                    }
                }
            }
        }
    }
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
