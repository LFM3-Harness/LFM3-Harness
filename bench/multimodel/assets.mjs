// Deterministic vision fixtures for the multi-model benchmark.
//
// The images are generated from code so the expected observation is fixed and
// the benchmark does not depend on any downloaded asset. Each image is a plain
// RGB PNG written with only Node's zlib.

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import zlib from "node:zlib";

const here = path.dirname(fileURLToPath(import.meta.url));
const outDir = path.join(here, "assets");

const CRC_TABLE = Array.from({ length: 256 }, (_, index) => {
  let value = index;
  for (let bit = 0; bit < 8; bit += 1) {
    value = value & 1 ? 0xedb88320 ^ (value >>> 1) : value >>> 1;
  }
  return value >>> 0;
});

function crc32(buffer) {
  let crc = 0xffffffff;
  for (const byte of buffer) {
    crc = CRC_TABLE[(crc ^ byte) & 0xff] ^ (crc >>> 8);
  }
  return (crc ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([length, body, crc]);
}

function encodePng(width, height, draw) {
  const pixels = Buffer.alloc(width * height * 3, 255);
  const paint = (x, y, color) => {
    if (x < 0 || y < 0 || x >= width || y >= height) return;
    const offset = (y * width + x) * 3;
    pixels[offset] = color[0];
    pixels[offset + 1] = color[1];
    pixels[offset + 2] = color[2];
  };
  const fillRect = (x0, y0, w, h, color) => {
    for (let y = y0; y < y0 + h; y += 1) {
      for (let x = x0; x < x0 + w; x += 1) paint(x, y, color);
    }
  };
  const fillCircle = (cx, cy, radius, color) => {
    for (let y = cy - radius; y <= cy + radius; y += 1) {
      for (let x = cx - radius; x <= cx + radius; x += 1) {
        if ((x - cx) ** 2 + (y - cy) ** 2 <= radius ** 2) paint(x, y, color);
      }
    }
  };
  draw({ fillRect, fillCircle });

  const raw = Buffer.alloc((width * 3 + 1) * height);
  for (let y = 0; y < height; y += 1) {
    const row = y * (width * 3 + 1);
    raw[row] = 0;
    pixels.copy(raw, row + 1, y * width * 3, (y + 1) * width * 3);
  }
  const header = Buffer.alloc(13);
  header.writeUInt32BE(width, 0);
  header.writeUInt32BE(height, 4);
  header[8] = 8;
  header[9] = 2;
  header[10] = 0;
  header[11] = 0;
  header[12] = 0;
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", header),
    chunk("IDAT", zlib.deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

// 5x7 bitmap glyphs for the two digits the number image shows.
const GLYPHS = {
  "4": ["10001", "10001", "10001", "11111", "00001", "00001", "00001"],
  "2": ["01110", "10001", "00001", "00010", "00100", "01000", "11111"],
};

function drawDigits(fillRect, text, originX, originY, scale, color) {
  let cursor = originX;
  for (const character of text) {
    const glyph = GLYPHS[character];
    if (!glyph) throw new Error(`no glyph for ${character}`);
    glyph.forEach((row, rowIndex) => {
      [...row].forEach((cell, columnIndex) => {
        if (cell === "1") {
          fillRect(cursor + columnIndex * scale, originY + rowIndex * scale, scale, scale, color);
        }
      });
    });
    cursor += (glyph[0].length + 1) * scale;
  }
}

const BLACK = [16, 16, 16];
const RED = [210, 30, 35];
const BLUE = [30, 70, 205];

function write(name, buffer) {
  fs.writeFileSync(path.join(outDir, name), buffer);
  return path.join(outDir, name);
}

fs.mkdirSync(outDir, { recursive: true });

const number = encodePng(480, 480, ({ fillRect }) => {
  drawDigits(fillRect, "42", 100, 156, 24, BLACK);
});
const shapes = encodePng(480, 480, ({ fillCircle, fillRect }) => {
  fillCircle(150, 240, 95, RED);
  fillRect(270, 145, 180, 180, BLUE);
});

console.log(write("vision-number.png", number));
console.log(write("vision-shapes.png", shapes));
