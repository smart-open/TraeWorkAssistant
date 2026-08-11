import { type ReactNode } from 'react';

export default function PageHeader({
  title,
  desc,
  actions,
}: {
  title: string;
  desc?: string;
  actions?: ReactNode;
}) {
  return (
    <div className="mb-5 flex items-end justify-between gap-4">
      <div>
        <h1 className="text-xl font-semibold text-slate-800 dark:text-slate-100">{title}</h1>
        {desc && <p className="mt-1 text-sm text-slate-500 dark:text-slate-400">{desc}</p>}
      </div>
      {actions && <div className="flex items-center gap-2">{actions}</div>}
    </div>
  );
}
