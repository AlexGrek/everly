// Ground-floor temperature diffusion over a packed world-tile window.
//
// One invocation per tile. `src`/`dst` ping-pong between substeps (see
// `src/map/temperature_diffusion.rs`). Heat flows only between conducting
// tiles (`mask > 0`, i.e. walkable floor); walls/void insulate. Each step
// also relaxes slightly toward `ambient`. Window edges and insulator
// neighbours are no-flux (skipped), so heat is conserved apart from the
// deliberate ambient term.

struct Params {
    width: u32,
    height: u32,
    alpha: f32,    // diffusion rate per substep (<= 0.25 for explicit stability)
    beta: f32,     // ambient relaxation per substep
    ambient: f32,  // equilibrium temperature (°C)
    temp_min: f32,
    temp_max: f32,
}

@group(0) @binding(0) var<storage, read_write> src: array<f32>;
@group(0) @binding(1) var<storage, read_write> dst: array<f32>;
@group(0) @binding(2) var<storage, read> mask: array<f32>;
@group(0) @binding(3) var<uniform> params: Params;

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = params.width;
    let h = params.height;
    if (gid.x >= w || gid.y >= h) {
        return;
    }
    let idx = gid.y * w + gid.x;
    let c = src[idx];

    // Insulators (walls / void / unloaded) hold their value and don't exchange.
    if (mask[idx] <= 0.0) {
        dst[idx] = c;
        return;
    }

    var sum = 0.0;
    if (gid.x > 0u) {
        let n = idx - 1u;
        if (mask[n] > 0.0) { sum += src[n] - c; }
    }
    if (gid.x + 1u < w) {
        let n = idx + 1u;
        if (mask[n] > 0.0) { sum += src[n] - c; }
    }
    if (gid.y > 0u) {
        let n = idx - w;
        if (mask[n] > 0.0) { sum += src[n] - c; }
    }
    if (gid.y + 1u < h) {
        let n = idx + w;
        if (mask[n] > 0.0) { sum += src[n] - c; }
    }

    var v = c + params.alpha * sum;
    v = v + params.beta * (params.ambient - v);
    dst[idx] = clamp(v, params.temp_min, params.temp_max);
}
