SEC("xdp") int rgnix_xdp(struct xdp_md *ctx) {
#ifdef DISPATCHER
    const struct settings *cfg = &dispatcher_settings;
#else
    u32 zero = 0;
    struct owner *owner = map_lookup(&rgnix_owner, &zero);
    struct settings *cfg = map_lookup(&rgnix_config, &zero);
    if (!owner || owner->magic != 0x52474e4958584432ULL || !cfg) return 2;
#endif
    struct packet packet = {};
    int parsed = parse(ctx, &packet);
#ifdef DISPATCHER
    int destination = dispatcher_destination(&packet);
#else
    int destination = 1;
#endif
    if (!destination || !in_scope(&packet, cfg)) { count(5); count(0); record_rule(1, 0, &packet); return 2; }
    int verdict;
    if (!parsed) { count(2); packet.reason = 1; verdict = 2 * 4 + cfg->malformed; }
    else if (parsed < 0) { packet.reason = 2; verdict = 3 * 4 + cfg->unsupported; }
    else if (packet.fragmented && cfg->fragments != 0) { packet.reason = 3; verdict = 4 * 4 + cfg->fragments; }
    else if (KEYED_RATES && !token_rate(&packet, cfg->ceiling_id, 0, 0, 0, cfg->ceiling_rate, cfg->ceiling_burst, 1, 1)) verdict = 5 * 4 + 1;
    else verdict = policy(&packet);
    u32 rule = verdict >> 2;
    if ((verdict & 3) == 1) {
        record_rule(rule, cfg->observe ? 2 : 1, &packet);
        sample_drop(&packet, cfg, rule);
        if (cfg->observe) { count(4); count(0); return 2; }
        count(1); return 1;
    }
    record_rule(rule, 0, &packet); count(0); return 2;
}
