import { deflateSync } from 'node:zlib'
import { mkdirSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'

/**
 * Generates the app icon without any image library: a battery glyph drawn
 * pixel by pixel, encoded as PNG. The menu-bar rings are drawn by the app itself.
 */

const CRC_TABLE = (() => {
  const table = new Int32Array(256)
  for (let n = 0; n < 256; n += 1) {
    let c = n
    for (let k = 0; k < 8; k += 1) {
      c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1
    }
    table[n] = c
  }
  return table
})()

function crc32(buf) {
  let crc = -1
  for (const byte of buf) {
    crc = CRC_TABLE[(crc ^ byte) & 0xff] ^ (crc >>> 8)
  }
  return (crc ^ -1) >>> 0
}

function chunk(type, data) {
  const length = Buffer.alloc(4)
  length.writeUInt32BE(data.length)
  const typeBuf = Buffer.from(type, 'ascii')
  const crc = Buffer.alloc(4)
  crc.writeUInt32BE(crc32(Buffer.concat([typeBuf, data])))
  return Buffer.concat([length, typeBuf, data, crc])
}

function encodePng(size, pixel) {
  const raw = Buffer.alloc(size * (size * 4 + 1))
  for (let y = 0; y < size; y += 1) {
    const rowStart = y * (size * 4 + 1)
    raw[rowStart] = 0
    for (let x = 0; x < size; x += 1) {
      const [r, g, b, a] = pixel(x, y)
      const offset = rowStart + 1 + x * 4
      raw[offset] = r
      raw[offset + 1] = g
      raw[offset + 2] = b
      raw[offset + 3] = a
    }
  }
  const ihdr = Buffer.alloc(13)
  ihdr.writeUInt32BE(size, 0)
  ihdr.writeUInt32BE(size, 4)
  ihdr[8] = 8
  ihdr[9] = 6
  ihdr[10] = 0
  ihdr[11] = 0
  ihdr[12] = 0
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', ihdr),
    chunk('IDAT', deflateSync(raw, { level: 9 })),
    chunk('IEND', Buffer.alloc(0))
  ])
}

/** Battery outline with a filled segment — the app icon glyph. */
function batteryPixel(size, fillRatio, opaque, transparent) {
  const scale = size / 32
  const bodyLeft = 3 * scale
  const bodyRight = 25 * scale
  const bodyTop = 8 * scale
  const bodyBottom = 24 * scale
  const stroke = Math.max(1, 2.2 * scale)
  const capLeft = bodyRight
  const capRight = 28.5 * scale
  const capTop = 12 * scale
  const capBottom = 20 * scale
  const fillRight = bodyLeft + stroke + (bodyRight - bodyLeft - stroke * 2) * fillRatio

  return (x, y) => {
    const insideBody =
      x >= bodyLeft - stroke / 2 &&
      x <= bodyRight + stroke / 2 &&
      y >= bodyTop - stroke / 2 &&
      y <= bodyBottom + stroke / 2
    if (insideBody) {
      const onOutline =
        Math.abs(x - bodyLeft) < stroke / 2 ||
        Math.abs(x - bodyRight) < stroke / 2 ||
        Math.abs(y - bodyTop) < stroke / 2 ||
        Math.abs(y - bodyBottom) < stroke / 2
      if (onOutline) {
        return opaque
      }
      const insideFill =
        x > bodyLeft + stroke * 0.6 &&
        x < fillRight &&
        y > bodyTop + stroke * 0.8 &&
        y < bodyBottom - stroke * 0.8
      if (insideFill) {
        return opaque
      }
      return transparent
    }
    if (x >= capLeft && x <= capRight && y >= capTop && y <= capBottom) {
      return opaque
    }
    return transparent
  }
}

export function writeAppIcon(dir) {
  mkdirSync(dir, { recursive: true })
  const white = [255, 255, 255, 255]
  const clear = [0, 0, 0, 0]
  const size = 1024
  const glyph = batteryPixel(size, 0.62, white, clear)
  const png = encodePng(size, (x, y) => {
    const cx = size / 2 - 0.5
    const cy = size / 2 - 0.5
    const dx = x - cx
    const dy = y - cy
    const distance = Math.sqrt(dx * dx + dy * dy)
    const radius = size * 0.47
    if (distance > radius) {
      return clear
    }
    const shade = 1 - distance / radius
    const background = [
      Math.round(37 + 34 * shade),
      Math.round(30 + 22 * shade),
      Math.round(74 + 46 * shade),
      255
    ]
    const inGlyph = glyph(x, y)
    if (inGlyph[3] > 0) {
      return white
    }
    return background
  })
  writeFileSync(join(dir, 'icon.png'), png)
}

if (process.argv[1]?.endsWith('make-icons.mjs')) {
  const target = process.argv[2] ?? 'assets'
  writeAppIcon(target)
  console.log(`icons written to ${target}`)
}
