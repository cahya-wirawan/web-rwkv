@group(0) @binding(0) var<uniform> shape: vec4<u32>;                        // [C, T, B]
#ifdef FP16
@group(0) @binding(1) var<storage, read_write> x: array<vec2<u32>>;         // (B, T, C)
#else
@group(0) @binding(1) var<storage, read_write> x: array<vec4<f32>>;         // (B, T, C)
#endif

fn pack4x16float(x: vec4<f32>) -> vec2<u32> {
    return vec2<u32>(pack2x16float(x.xy), pack2x16float(x.zw));
}

fn unpack4x16float(x: vec2<u32>) -> vec4<f32> {
    return vec4<f32>(unpack2x16float(x.x), unpack2x16float(x.y));
}

@compute @workgroup_size(BLOCK_SIZE, 1, 1)
fn squared_relu(@builtin(global_invocation_id) invocation_id: vec3<u32>) {
    let stride = shape[0] / 4u;
    let index = invocation_id.x;
    let token = invocation_id.y;
    let batch = invocation_id.z;

    if index < stride {
        let bti = (batch * shape[1] + token) * stride + index;
#ifdef FP16
        let p = max(unpack4x16float(x[bti]), vec4<f32>(0.0));
        x[bti] = pack4x16float(p * p);
#else
        let p = max(x[bti], vec4<f32>(0.0));
        x[bti] = p * p;
#endif
    }
}

@compute @workgroup_size(BLOCK_SIZE, 1, 1)
fn act_tanh(@builtin(global_invocation_id) invocation_id: vec3<u32>) {
    let stride = shape[0] / 4u;
    let index = invocation_id.x;
    let token = invocation_id.y;
    let batch = invocation_id.z;

    if index < stride {
        let bti = (batch * shape[1] + token) * stride + index;
#ifdef FP16
        x[bti] = pack4x16float(tanh(unpack4x16float(x[bti])));
#else
        x[bti] = tanh(x[bti]);
#endif
    }
}

@compute @workgroup_size(BLOCK_SIZE, 1, 1)
fn stable_exp(@builtin(global_invocation_id) invocation_id: vec3<u32>) {
    let stride = shape[0] / 4u;
    let index = invocation_id.x;
    let token = invocation_id.y;
    let batch = invocation_id.z;

    if index < stride {
        let bti = (batch * shape[1] + token) * stride + index;
#ifdef FP16
        x[bti] = pack4x16float(exp(-exp(unpack4x16float(x[bti]))));
#else
        x[bti] = exp(-exp(x[bti]));
#endif
    }
}

@compute @workgroup_size(BLOCK_SIZE, 1, 1)
fn opposite_exp(@builtin(global_invocation_id) invocation_id: vec3<u32>) {
    let stride = shape[0] / 4u;
    let index = invocation_id.x;
    let token = invocation_id.y;
    let batch = invocation_id.z;

    if index < stride {
        let bti = (batch * shape[1] + token) * stride + index;
#ifdef FP16
        x[bti] = -pack4x16float(exp(x[bti]));
#else
        x[bti] = -exp(x[bti]);
#endif
    }
}
