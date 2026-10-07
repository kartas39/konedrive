#pragma once

#include <QObject>

/// "Show a tray icon for each account": with several accounts, whether each
/// has its own tray icon or one icon stands for them all. Stored the same way
/// PlacesSettings is, under `TrayIconPerAccount` in konedriverc's [General]
/// group; on by default.
class TraySettings : public QObject
{
    Q_OBJECT
    Q_PROPERTY(bool perAccount READ perAccount WRITE setPerAccount NOTIFY perAccountChanged)

public:
    explicit TraySettings(QObject *parent = nullptr);

    bool perAccount() const { return m_perAccount; }
    void setPerAccount(bool perAccount);

Q_SIGNALS:
    void perAccountChanged();

private:
    bool m_perAccount;
};
