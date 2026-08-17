package com.bluedragonmc.steelworldgen;

/**
 * The dimension to generate chunks for.
 */
public enum Dimension {
    OVERWORLD(0),
    NETHER(1),
    THE_END(2);

    private final byte id;

    Dimension(int id) {
        this.id = (byte) id;
    }

    public byte getId() {
        return id;
    }
}
