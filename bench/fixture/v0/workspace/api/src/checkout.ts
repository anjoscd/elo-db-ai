import { invoice } from '@lantern/billing';
export const checkout = (cents: number) => invoice(cents, 'BRL');
