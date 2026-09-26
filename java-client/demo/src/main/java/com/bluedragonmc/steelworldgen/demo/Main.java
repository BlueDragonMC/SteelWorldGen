package com.bluedragonmc.steelworldgen.demo;

import com.bluedragonmc.steelworldgen.Dimension;
import com.bluedragonmc.steelworldgen.SteelWorldGenProvider;
import net.minestom.server.MinecraftServer;
import net.minestom.server.command.builder.Command;
import net.minestom.server.coordinate.Pos;
import net.minestom.server.entity.GameMode;
import net.minestom.server.entity.Player;
import net.minestom.server.event.player.AsyncPlayerConfigurationEvent;
import net.minestom.server.instance.Instance;
import net.minestom.server.instance.LightingChunk;
import net.minestom.server.world.DimensionType;

public class Main {
    static void main() {
        MinecraftServer server = MinecraftServer.init();

        Instance overworld = MinecraftServer.getInstanceManager().createInstanceContainer();
        overworld.setGenerator(SteelWorldGenProvider.getGenerator(42L));
        overworld.setChunkSupplier(LightingChunk::new);

        Instance nether = MinecraftServer.getInstanceManager().createInstanceContainer(DimensionType.THE_NETHER);
        nether.setGenerator(SteelWorldGenProvider.getGenerator(42L, Dimension.NETHER));
        nether.setChunkSupplier(LightingChunk::new);

        Instance theEnd = MinecraftServer.getInstanceManager().createInstanceContainer(DimensionType.THE_END);
        theEnd.setGenerator(SteelWorldGenProvider.getGenerator(42L, Dimension.THE_END));
        theEnd.setChunkSupplier(LightingChunk::new);

        MinecraftServer.getGlobalEventHandler().addListener(AsyncPlayerConfigurationEvent.class, (event) -> {
            event.setSpawningInstance(overworld);
            event.getPlayer().setRespawnPoint(new Pos(0, 64, 0));
            event.getPlayer().setGameMode(GameMode.SPECTATOR);
        });

        Command overworldCommand = new Command("overworld");
        overworldCommand.setDefaultExecutor((sender, _) -> {
            if (!(sender instanceof Player p)) return;
            p.setInstance(overworld);
        });

        Command netherCommand = new Command("nether");
        netherCommand.setDefaultExecutor((sender, _) -> {
            if (!(sender instanceof Player p)) return;
            p.setInstance(nether);
        });

        Command theEndCommand = new Command("end");
        theEndCommand.setDefaultExecutor((sender, _) -> {
            if (!(sender instanceof Player p)) return;
            p.setInstance(theEnd);
        });

        MinecraftServer.getCommandManager().register(overworldCommand, netherCommand, theEndCommand);

        server.start("0.0.0.0", 25565);
    }
}
