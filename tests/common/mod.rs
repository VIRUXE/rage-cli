//! Hand-built synthetic assets shared by CLI integration tests.
pub fn triangle_dff() -> Vec<u8> {
    fn words(values: &[u32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }
    fn floats(values: &[f32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }
    fn chunk(kind: u32, data: &[u8]) -> Vec<u8> {
        let mut out = words(&[kind, data.len() as u32, 0x1803ffff]);
        out.extend_from_slice(data);
        out
    }
    let mut frame = words(&[1]);
    frame.extend(floats(&[1., 0., 0., 0., 1., 0., 0., 0., 1., 0., 0., 0.]));
    frame.extend(words(&[u32::MAX, 0]));
    let mut frames = chunk(1, &frame);
    frames.extend(chunk(3, &chunk(0x253f2fe, b"triangle")));
    let mut data = words(&[0x10016, 1, 3, 1]);
    data.extend(floats(&[0., 0., 1., 0., 0., 1.]));
    for n in [1u16, 0, 0, 2] {
        data.extend_from_slice(&n.to_le_bytes());
    }
    data.extend(floats(&[0., 0., 0., 2.]));
    data.extend(words(&[1, 1]));
    data.extend(floats(&[0., 0., 0., 1., 0., 0., 0., 1., 0.]));
    data.extend(floats(&[0., 0., 1.].repeat(3)));
    let mut m = words(&[0]);
    m.extend([255; 4]);
    m.extend(words(&[0, 1]));
    m.extend(floats(&[1., 0., 1.]));
    let mut tex = chunk(1, &words(&[0x1106]));
    tex.extend(chunk(2, b"diffuse\0"));
    tex.extend(chunk(2, &[0; 4]));
    tex.extend(chunk(3, &[]));
    let mut material = chunk(1, &m);
    material.extend(chunk(6, &tex));
    material.extend(chunk(3, &[]));
    let mut materials = chunk(1, &words(&[1, u32::MAX]));
    materials.extend(chunk(7, &material));
    let mut geom = chunk(1, &data);
    geom.extend(chunk(8, &materials));
    geom.extend(chunk(3, &chunk(0x50e, &words(&[0, 1, 3, 3, 0, 0, 1, 2]))));
    let mut geoms = chunk(1, &words(&[1]));
    geoms.extend(chunk(0xf, &geom));
    let mut atomic = chunk(1, &words(&[0, 0, 5, 0]));
    atomic.extend(chunk(3, &[]));
    let mut root = chunk(1, &words(&[1, 0, 0]));
    root.extend(chunk(0xe, &frames));
    root.extend(chunk(0x1a, &geoms));
    root.extend(chunk(0x14, &atomic));
    root.extend(chunk(3, &[]));
    let mut bytes = chunk(0x10, &root);
    bytes.resize(2048, 0);
    bytes
}
