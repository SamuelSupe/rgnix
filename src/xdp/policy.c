struct rate_key { u64 id; u8 address[16]; u32 version; u16 port, pad; };
struct set_key { u32 prefix, set, version; u8 address[16]; };
struct set_value { u64 expires; u32 prefix, pad; };
struct rule_count { u64 packets, bytes; };
struct settings {
    u32 observe, malformed, unsupported, fragments;
    u32 destinations, port_count, protocol_count, sample_every;
    u32 event_rate, ceiling_rate, ceiling_burst, pad;
    u64 ceiling_id;
    u16 ports[32]; u8 protocols[16];
};
struct owner { u64 magic, netns; u32 ifindex, mode; };
struct event { u64 timestamp; u32 length, rule, reason, observed; u8 src[16], dst[16]; u16 src_port, dst_port; u8 version, protocol; u16 pad; };
MAP(rgnix_owner, 2, u32, struct owner, 1, 0);
MAP(rgnix_config, 2, u32, struct settings, 1, 0);
MAP(rgnix_rates, 1, u64, u64, 256, 0);
MAP(rgnix_keys, 9, struct rate_key, u64, 65536, 0);
MAP(rgnix_sets, 11, struct set_key, struct set_value, 16416, 1);
MAP(rgnix_rules, 6, u32, struct rule_count, 408, 0);
struct { int (*type)[27]; int (*max_entries)[262144]; } rgnix_events SEC(".maps");
static long (*map_update)(void *, const void *, const void *, u64) = (void *)2;
static long (*ring_output)(void *, void *, u64, u64) = (void *)130;
static u32 (*random_u32)(void) = (void *)7;

static __always_inline int bucket(u64 *state, u64 now, u32 rate, u32 burst, u32 cost, struct packet *p, int account) {
    if (!state) { if (account) count(7); p->reason = 7; return 0; }
    // GCRA encodes the remaining token debt in one atomic timestamp. Rounding up
    // intentionally errs towards throttling; reloads share this state by rule ID.
    u64 interval = (1000000000ULL + rate - 1) / rate;
    u64 allowance = (u64)burst * interval;
    if (cost > burst) { if (account) count(3); p->reason = 4; return 0; }
#pragma unroll
    for (int i = 0; i < 8; i++) {
        u64 old = *(volatile u64 *)state;
        u64 next = (old > now ? old : now) + (u64)cost * interval;
        if (next - now > allowance) { if (account) count(3); p->reason = 4; return 0; }
        if (__sync_val_compare_and_swap(state, old, next) == old) return 1;
    }
    if (account) count(6); p->reason = 6; return 0;
}
static __always_inline int token_rate(struct packet *p, u64 id, u32 kind, u32 prefix4, u32 prefix6, u32 rate, u32 burst, u32 cost, int account) {
    u64 now = ktime_ns(), initial = now;
    if (!kind) {
        u64 *state = map_lookup(&rgnix_rates, &id);
        if (!state) { map_update(&rgnix_rates, &id, &initial, 1); state = map_lookup(&rgnix_rates, &id); }
        return bucket(state, now, rate, burst, cost, p, account);
    }
    struct rate_key key = { .id = id, .version = p->version };
    if (kind == 1 || kind == 3) {
        __builtin_memcpy(key.address, p->src, 16);
        if (p->version == 4 && prefix4 == 24) key.address[3] = 0;
        if (p->version == 6 && prefix6 == 64) __builtin_memset(key.address + 8, 0, 8);
    }
    if (kind == 2 || kind == 3) key.port = p->dst_port;
    u64 *state = map_lookup(&rgnix_keys, &key);
    if (!state) {
        if (!map_update(&rgnix_keys, &key, &initial, 1)) count(8);
        state = map_lookup(&rgnix_keys, &key);
    }
    return bucket(state, now, rate, burst, cost, p, account);
}
static __always_inline int in_set(struct packet *p, u32 set, u32 destination) {
    if (p->version != 4 && p->version != 6) return 0;
    struct set_key key = { .prefix = p->version == 4 ? 96 : 192, .set = set, .version = p->version };
    if (destination) __builtin_memcpy(key.address, p->dst, 16);
    else __builtin_memcpy(key.address, p->src, 16);
    u64 now = 0;
    // Expiring a more-specific entry must not hide an unexpired covering prefix.
#pragma clang loop unroll(disable)
    for (int i = 0; i < 129; i++) {
        struct set_value *value = map_lookup(&rgnix_sets, &key);
        if (!value) return 0;
        if (!value->expires) return 1;
        if (!now) now = ktime_ns();
        if (value->expires > now) return 1;
        if (value->prefix <= 64 || value->prefix > 192) return 0;
        key.prefix = value->prefix - 1;
    }
    return 0;
}
static __always_inline int in_scope(struct packet *p, const struct settings *cfg) {
    if (cfg->destinations && !in_set(p, 0xffffffff, 1)) return 0;
    if (cfg->port_count) {
        if (!p->ports) return 0;
        int found = 0;
#pragma clang loop unroll(disable)
        for (u32 i = 0; i < 32 && i < cfg->port_count; i++) {
            if (p->dst_port == cfg->ports[i]) { found = 1; break; }
        }
        if (!found) return 0;
    }
    if (cfg->protocol_count) {
        int found = 0;
#pragma clang loop unroll(disable)
        for (u32 i = 0; i < 16 && i < cfg->protocol_count; i++) {
            if (p->protocol == cfg->protocols[i]) { found = 1; break; }
        }
        if (!found) return 0;
    }
    return 1;
}
static __always_inline void record_rule(u32 rule, u32 action, struct packet *p) {
    u32 key = rule * 3 + action;
    struct rule_count *value = map_lookup(&rgnix_rules, &key);
    if (value) { value->packets++; value->bytes += p->length; }
}
static __always_inline void sample_drop(struct packet *p, const struct settings *cfg, u32 rule) {
    if (!cfg->sample_every || !cfg->event_rate || random_u32() % cfg->sample_every) return;
    u8 reason = p->reason;
    if (!token_rate(p, 0, 0, 0, 0, cfg->event_rate, cfg->event_rate, 1, 0)) { p->reason = reason; count(9); return; }
    struct event event = { .timestamp = ktime_ns(), .length = p->length, .rule = rule, .reason = reason,
        .observed = cfg->observe, .src_port = p->src_port, .dst_port = p->dst_port, .version = p->version, .protocol = p->protocol };
    __builtin_memcpy(event.src, p->src, 16); __builtin_memcpy(event.dst, p->dst, 16);
    if (ring_output(&rgnix_events, &event, sizeof(event), 0)) count(9);
}
