/** @type {import('tailwindcss').Config} */
export default {
  content: ['./index.html', './src/**/*.{ts,tsx}'],
  theme: {
    extend: {
      colors: {
        // Trading-terminal palette: near-black ground, restrained accents.
        term: {
          bg: '#0a0d12',
          panel: '#111621',
          border: '#1e2635',
          hover: '#161d2b',
          text: '#d6dee9',
          muted: '#7c8899',
          dim: '#4a5566',
        },
        up: '#00d68f',
        down: '#ff4d6a',
        warn: '#ffb020',
        info: '#3d8bfd',
      },
      fontFamily: {
        mono: ['JetBrains Mono', 'SF Mono', 'Menlo', 'Consolas', 'monospace'],
      },
      fontSize: { '2xs': '0.6875rem' },
    },
  },
  plugins: [],
}
