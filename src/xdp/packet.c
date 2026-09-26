typedef unsigned char u8;
typedef unsigned short u16;
typedef unsigned int u32;
typedef unsigned long long u64;
#define SEC(name) __attribute__((section(name), used))
#define __always_inline inline __attribute__((always_inline))
struct xdp_md { u32 data, data_end, data_meta, ingress_ifindex, rx_queue_index, egress_ifindex; };
#define MAP(name, kind, key_t, value_t, capacity, flags) \
struct { int (*type)[kind]; key_t *key; value_t *value; int (*max_entries)[capacity]; int (*map_flags)[flags]; } name SEC(".maps")
MAP(rgnix_stats, 6, u32, u64, 16, 0);
MAP(rgnix_legacy, 2, u32, u64, 64, 0);
static void *(*map_lookup)(void *, const void *) = (void *)1;
static u64 (*ktime_ns)(void) = (void *)5;
char SEC("license") rgnix_license[] = "GPL";
struct packet {
    u8 src[16], dst[16];
    u32 length;
    u16 src_port, dst_port;
    u8 version, protocol, tcp_flags, fragmented, ports, reason;
};
static __always_inline void count(u32 key) {
    u64 *value = map_lookup(&rgnix_stats, &key);
    if (value) (*value)++;
}
static __always_inline int allow_rate(u32 key, u32 limit) {
    u64 *value = map_lookup(&rgnix_legacy, &key);
    if (!value) return 0;
    u64 epoch = (ktime_ns() / 1000000000ULL) << 32;
    // A single CAS updates both epoch and count across CPUs. Contention fails closed.
#pragma unroll
    for (int i = 0; i < 8; i++) {
        u64 previous = *(volatile u64 *)value;
        u32 used = (previous & 0xffffffff00000000ULL) == epoch ? (u32)previous : 0;
        if (used >= limit) { count(3); return 0; }
        if (__sync_val_compare_and_swap(value, previous, epoch | (used + 1)) == previous) return 1;
    }
    count(3);
    return 0;
}
static __always_inline u16 be16(const u8 *p) { return ((u16)p[0] << 8) | p[1]; }
static __always_inline int parse(struct xdp_md *ctx, struct packet *p) {
    u8 *data = (void *)(unsigned long)ctx->data;
    u8 *end = (void *)(unsigned long)ctx->data_end;
    p->length = end - data;
    if (data + 14 > end) return 0;
    u16 type = be16(data + 12);
    u8 *cursor = data + 14;
#pragma unroll
    for (int i = 0; i < 2; i++) {
        if (type == 0x8100 || type == 0x88a8) {
            if (cursor + 4 > end) return 0;
            type = be16(cursor + 2);
            cursor += 4;
        }
    }
    if (type == 0x8100 || type == 0x88a8) return -1;
    u8 *ip_end = end;
    if (type == 0x0800) {
        if (cursor + 20 > end || (cursor[0] >> 4) != 4) return 0;
        u32 size = (cursor[0] & 15) * 4;
        u16 total = be16(cursor + 2);
        if (size < 20 || total < size || cursor + total > end || cursor + size > end) return 0;
        ip_end = cursor + total;
        p->version = 4;
        p->protocol = cursor[9];
        p->fragmented = (be16(cursor + 6) & 0x3fff) != 0;
        __builtin_memcpy(p->src, cursor + 12, 4);
        __builtin_memcpy(p->dst, cursor + 16, 4);
        cursor += size;
    } else if (type == 0x86dd) {
        if (cursor + 40 > end || (cursor[0] >> 4) != 6) return 0;
        u16 payload = be16(cursor + 4);
        if (cursor + 40 + payload > end) return 0;
        ip_end = cursor + 40 + payload;
        p->version = 6;
        p->protocol = cursor[6];
        __builtin_memcpy(p->src, cursor + 8, 16);
        __builtin_memcpy(p->dst, cursor + 24, 16);
        cursor += 40;
#pragma unroll
        for (int i = 0; i < 8; i++) {
            // LLVM must retain the actual data_end checks. The BPF verifier cannot
            // infer readable bytes from comparisons against our logical IP end.
            asm volatile("" : "+r"(end));
            if (p->protocol == 44) {
                if (cursor + 8 > end || cursor + 8 > ip_end) return 0;
                p->fragmented = 1;
                p->protocol = cursor[0];
                return 1;
            }
            if (p->protocol != 0 && p->protocol != 43 && p->protocol != 60 && p->protocol != 51) break;
            if (cursor + 2 > end || cursor + 2 > ip_end) return 0;
            u32 size = p->protocol == 51 ? ((u32)cursor[1] + 2) * 4 : ((u32)cursor[1] + 1) * 8;
            p->protocol = cursor[0];
            if (cursor + size > end || cursor + size > ip_end) return 0;
            cursor += size;
        }
        if (p->protocol == 0 || p->protocol == 43 || p->protocol == 60 || p->protocol == 51 || p->protocol == 44) return -1;
    } else {
        return 1;
    }
    if (p->fragmented) return 1;
    asm volatile("" : "+r"(end));
    if (p->protocol == 6) {
        if (cursor + 20 > end || cursor + 20 > ip_end) return 0;
        u32 size = (cursor[12] >> 4) * 4;
        if (size < 20 || cursor + size > end || cursor + size > ip_end) return 0;
        p->tcp_flags = cursor[13];
    } else if (p->protocol == 17) {
        if (cursor + 8 > end || cursor + 8 > ip_end) return 0;
        u16 size = be16(cursor + 4);
        if (size < 8 || cursor + size > end || cursor + size > ip_end) return 0;
    } else {
        return 1;
    }
    p->src_port = be16(cursor);
    p->dst_port = be16(cursor + 2);
    p->ports = 1;
    return 1;
}
