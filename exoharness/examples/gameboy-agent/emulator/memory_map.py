"""Pokemon Red/Blue (English) RAM decoding.

Reads well-documented WRAM addresses into a structured game-state dict so the
agent harness gets objective progress data (position, party, badges, money,
Pokedex) that does not depend on the vision model reading sprite text.

Addresses follow the pret/pokered disassembly symbol map
(https://github.com/pret/pokered — see wram.asm and the generated .sym
file); the Data Crystal RAM map is a readable cross-reference
(https://datacrystal.tcrf.net/wiki/Pok%C3%A9mon_Red_and_Blue/RAM_map).
Name tables below are partial (display-only convenience); unknown ids fall
back to hex placeholders.
"""

from __future__ import annotations

W_CUR_MAP = 0xD35E
W_Y_COORD = 0xD361
W_X_COORD = 0xD362
PLAYER_FACING = 0xC109  # sprite $C1x9: 0 down, 4 up, 8 left, 0xC right
W_IS_IN_BATTLE = 0xD057  # 0 none, 1 wild, 2 trainer, 0xFF lost
W_OBTAINED_BADGES = 0xD356
W_PLAYER_MONEY = 0xD347  # 3 bytes, binary-coded decimal
W_PARTY_COUNT = 0xD163
W_PARTY_SPECIES = 0xD164  # up to 6 entries, 0xFF terminated
W_PARTY_MONS = 0xD16B  # 44-byte structs
PARTY_MON_SIZE = 44
W_POKEDEX_OWNED = 0xD2F7  # 19-byte bitfield
POKEDEX_OWNED_BYTES = 19

FACING_NAMES = {0x00: "down", 0x04: "up", 0x08: "left", 0x0C: "right"}

BADGE_NAMES = [
    "Boulder",
    "Cascade",
    "Thunder",
    "Rainbow",
    "Soul",
    "Marsh",
    "Volcano",
    "Earth",
]

# Partial internal-index -> species name table (Gen 1 internal ids, not dex
# numbers). Covers starters and common early-game encounters; display-only.
SPECIES_NAMES = {
    0x01: "Rhydon",
    0x03: "NidoranM",
    0x04: "Clefairy",
    0x05: "Spearow",
    0x09: "Ivysaur",
    0x0F: "NidoranF",
    0x1C: "Blastoise",
    0x22: "Onix",
    0x24: "Pidgey",
    0x25: "Slowpoke",
    0x39: "Mankey",
    0x54: "Pikachu",
    0x55: "Raichu",
    0x70: "Weedle",
    0x71: "Kakuna",
    0x72: "Beedrill",
    0x7B: "Caterpie",
    0x7C: "Metapod",
    0x7D: "Butterfree",
    0x96: "Pidgeotto",
    0x97: "Pidgeot",
    0x99: "Bulbasaur",
    0x9A: "Venusaur",
    0xA5: "Rattata",
    0xA6: "Raticate",
    0xB0: "Charmander",
    0xB1: "Squirtle",
    0xB2: "Charmeleon",
    0xB3: "Wartortle",
    0xB4: "Charizard",
}

# Partial map-id -> name table for early-game Kanto; display-only.
MAP_NAMES = {
    0x00: "Pallet Town",
    0x01: "Viridian City",
    0x02: "Pewter City",
    0x03: "Cerulean City",
    0x04: "Lavender Town",
    0x05: "Vermilion City",
    0x06: "Celadon City",
    0x07: "Fuchsia City",
    0x08: "Cinnabar Island",
    0x09: "Indigo Plateau",
    0x0A: "Saffron City",
    0x0C: "Route 1",
    0x0D: "Route 2",
    0x0E: "Route 3",
    0x0F: "Route 4",
    0x10: "Route 5",
    0x11: "Route 6",
    0x12: "Route 7",
    0x13: "Route 8",
    0x14: "Route 9",
    0x15: "Route 10",
    0x16: "Route 11",
    0x17: "Route 12",
    0x18: "Route 13",
    0x19: "Route 14",
    0x1A: "Route 15",
    0x1B: "Route 16",
    0x1C: "Route 17",
    0x1D: "Route 18",
    0x1E: "Route 19",
    0x1F: "Route 20",
    0x20: "Route 21",
    0x21: "Route 22",
    0x22: "Route 23",
    0x23: "Route 24",
    0x24: "Route 25",
    0x25: "Player's House 1F",
    0x26: "Player's House 2F",
    0x27: "Rival's House",
    0x28: "Oak's Lab",
    0x29: "Viridian Pokecenter",
    0x2A: "Viridian Mart",
    0x2B: "Viridian School",
    0x2C: "Viridian House",
    0x2D: "Viridian Gym",
    0x2E: "Diglett's Cave (Route 2 side)",
    0x2F: "Viridian Forest North Gate",
    0x31: "Route 2 Gate",
    0x32: "Viridian Forest South Gate",
    0x33: "Viridian Forest",
    0x34: "Pewter Museum 1F",
    0x36: "Pewter Gym",
    0x38: "Pewter Mart",
    0x3A: "Pewter Pokecenter",
    0x3B: "Mt Moon 1F",
}


def species_name(species_id: int) -> str:
    return SPECIES_NAMES.get(species_id, f"SPECIES_0x{species_id:02X}")


def map_name(map_id: int) -> str:
    return MAP_NAMES.get(map_id, f"MAP_0x{map_id:02X}")


def _bcd(data: list[int]) -> int:
    value = 0
    for byte in data:
        value = value * 100 + ((byte >> 4) * 10) + (byte & 0x0F)
    return value


def _u16(hi: int, lo: int) -> int:
    return (hi << 8) | lo


def read_state(memory) -> dict:
    """Decode game state from a PyBoy memory view (supports slicing)."""
    map_id = memory[W_CUR_MAP]
    facing = memory[PLAYER_FACING]
    in_battle_raw = memory[W_IS_IN_BATTLE]
    badges = memory[W_OBTAINED_BADGES]
    money = _bcd(list(memory[W_PLAYER_MONEY : W_PLAYER_MONEY + 3]))
    party_count = min(memory[W_PARTY_COUNT], 6)

    party = []
    for i in range(party_count):
        base = W_PARTY_MONS + i * PARTY_MON_SIZE
        species = memory[W_PARTY_SPECIES + i]
        party.append(
            {
                "species": species_name(species),
                "level": memory[base + 33],
                "hp": _u16(memory[base + 1], memory[base + 2]),
                "max_hp": _u16(memory[base + 34], memory[base + 35]),
            }
        )

    owned_bits = list(memory[W_POKEDEX_OWNED : W_POKEDEX_OWNED + POKEDEX_OWNED_BYTES])
    pokedex_owned = sum(bin(byte).count("1") for byte in owned_bits)

    badge_list = [name for bit, name in enumerate(BADGE_NAMES) if badges & (1 << bit)]

    battle = "none"
    if in_battle_raw == 1:
        battle = "wild"
    elif in_battle_raw == 2:
        battle = "trainer"
    elif in_battle_raw == 0xFF:
        battle = "lost"

    return {
        "map_id": map_id,
        "map_name": map_name(map_id),
        "x": memory[W_X_COORD],
        "y": memory[W_Y_COORD],
        "facing": FACING_NAMES.get(facing & 0x0C, f"0x{facing:02X}"),
        "in_battle": battle,
        "badges": badge_list,
        "badge_count": len(badge_list),
        "money": money,
        "party": party,
        "pokedex_owned": pokedex_owned,
    }
