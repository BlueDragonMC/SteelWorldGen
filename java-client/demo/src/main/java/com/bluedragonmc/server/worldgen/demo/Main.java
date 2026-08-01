package com.bluedragonmc.server.worldgen.demo;

import com.bluedragonmc.server.worldgen.SteelWorldGenProvider;
import net.minestom.server.MinecraftServer;
import net.minestom.server.coordinate.Pos;
import net.minestom.server.entity.GameMode;
import net.minestom.server.event.player.AsyncPlayerConfigurationEvent;
import net.minestom.server.instance.Instance;
import net.minestom.server.instance.LightingChunk;

public class Main {
    static void main() {
        MinecraftServer server = MinecraftServer.init();

        Instance instance = MinecraftServer.getInstanceManager().createInstanceContainer();
        instance.setGenerator(SteelWorldGenProvider.getGenerator(42L));
        instance.setChunkSupplier(LightingChunk::new);

        MinecraftServer.getGlobalEventHandler().addListener(AsyncPlayerConfigurationEvent.class, (event) -> {
            event.setSpawningInstance(instance);
            event.getPlayer().setRespawnPoint(new Pos(0, 64, 0));
            event.getPlayer().setGameMode(GameMode.SPECTATOR);
        });

        server.start("0.0.0.0", 25565);
    }
}
