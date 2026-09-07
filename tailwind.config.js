/** @type {import('tailwindcss').Config} */
export default {
  content: ['./index.html', './src/**/*.{ts,tsx}'],
  darkMode: 'class',
  theme: {
    extend: {
      colors: {
        brand: {
          50: '#fafafa',
          100: '#f4f4f5',
          200: '#e4e4e7',
          300: '#d4d4d8',
          400: '#a1a1aa',
          500: '#71717a',
          600: '#52525b',
          700: '#3f3f46',
          800: '#27272a',
          900: '#18181b',
          950: '#09090b',
        },
        // slate / zinc 关键档位变量化：主题换肤通过 html[data-theme] 覆盖 CSS 变量实现
        // （见 index.css :root 与各主题覆盖块；默认值 = Tailwind 原色）
        slate: {
          50: 'rgb(var(--c-slate-50) / <alpha-value>)',
          100: 'rgb(var(--c-slate-100) / <alpha-value>)',
          200: 'rgb(var(--c-slate-200) / <alpha-value>)',
          300: 'rgb(var(--c-slate-300) / <alpha-value>)',
          400: '#94a3b8',
          500: '#64748b',
          600: '#475569',
          700: '#334155',
          800: '#1e293b',
          900: '#0f172a',
          950: '#020617',
        },
        zinc: {
          50: '#fafafa',
          100: 'rgb(var(--c-zinc-100) / <alpha-value>)',
          200: 'rgb(var(--c-zinc-200) / <alpha-value>)',
          300: '#d4d4d8',
          400: '#a1a1aa',
          500: '#71717a',
          600: '#52525b',
          700: 'rgb(var(--c-zinc-700) / <alpha-value>)',
          800: 'rgb(var(--c-zinc-800) / <alpha-value>)',
          900: 'rgb(var(--c-zinc-900) / <alpha-value>)',
          950: 'rgb(var(--c-zinc-950) / <alpha-value>)',
        },
      },
      fontFamily: {
        sans: [
          'Inter', 'system-ui', '-apple-system', 'Segoe UI', 'Roboto',
          '"PingFang SC"', '"Microsoft YaHei"', 'sans-serif',
        ],
      },
      boxShadow: {
        soft: '0 1px 2px rgba(0,0,0,0.04), 0 8px 24px -10px rgba(0,0,0,0.10)',
        'soft-lg': '0 2px 6px rgba(0,0,0,0.05), 0 20px 44px -14px rgba(0,0,0,0.18)',
      },
      keyframes: {
        'fade-in': {
          '0%': { opacity: '0', transform: 'translateY(4px)' },
          '100%': { opacity: '1', transform: 'translateY(0)' },
        },
        'pop-in': {
          '0%': { opacity: '0', transform: 'scale(0.98) translateY(6px)' },
          '100%': { opacity: '1', transform: 'scale(1) translateY(0)' },
        },
      },
      animation: {
        'fade-in': 'fade-in .18s ease-out',
        'pop-in': 'pop-in .26s cubic-bezier(0.22,1,0.36,1)',
      },
    },
  },
  plugins: [],
};
